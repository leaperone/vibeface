#!/usr/bin/env python3
"""Run local Node process acceptance without making paid provider calls.

The default run starts four Nodes over iroh (one runs the member relay, one
reaches others only through it) and checks sync, signed entrances, forwarding
through other Gateways, delivery to the scheduling Node without replay, a relay
outage and an entrance switch. Host work uses bot turns on a controlled fixture App Server, never a model. All state and logs stay beneath
--artifacts with private permissions. The script prints no credentials.
--phase1 checks durable Manager queues, scoped tools and cross-entry progress
using a controlled fixture App Server. It does not verify real provider model behavior.
--chat-agent checks direct App Server work through the Agent's Shell CLIs, request
handling, triage, webhooks and decisions, and native event recovery with real Node
processes and controlled provider protocol fixtures.
--web-only checks packaged Web resources through one isolated Node process.
--auto-route checks the entry: joins, origins, revocation, unknown writes, recovery and forwarding
through another member Gateway.
"""

import argparse
import base64
import copy
import hashlib
import hmac
import http.client
import http.server
import ipaddress
import json
import os
import re
import runpy
from html.parser import HTMLParser
from pathlib import Path
import shutil
import signal
import select
import socket
import struct
import stat
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


def definitions_of(bots):
    """Listed Bots without the read-only preview of their latest message and their busy state."""
    return [{key: value for key, value in bot.items() if key not in ("last_message", "busy")} for bot in bots]


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
    """Replaces the file whole, so a reader in another thread or process never sees it half written."""
    temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}")
    with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as output:
        output.write(json.dumps(value, indent=2) + "\n")
    os.replace(temporary, path)


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
                 lambda: stream.connect(f"/tmp/vibeface-{node.device_id[:32]}/maintenance.sock") is None,
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


_port_base = [int(os.environ.get("VIBEFACE_PORT_BASE", "0"))]


def available_port():
    """A free TCP/UDP loopback port; VIBEFACE_PORT_BASE pins a known range."""
    while True:
        port = _port_base[0]
        if port:
            _port_base[0] += 1
        with socket.socket() as stream, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as datagram:
            try:
                stream.bind(("127.0.0.1", port))
                port = stream.getsockname()[1]
                datagram.bind(("127.0.0.1", port))
                return port
            except OSError:
                continue


CLIENT_SOURCE = r"""
import init, { Identity, Handshake, signingMessage, recoverySeal, recoveryOpen, verifyMembership, createGenesis, governancePayload, inviteSigningPayload, joinCommit, shortCode, ticketEncode, ticketDecode, attestationPayload, verify } from "MODULE_URL";
import { lstat, readFile, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import { createHash, createPrivateKey, randomBytes, sign as edSign } from 'node:crypto';
await init({ module_or_path: await readFile(new URL('./vibeface_secure_bg.wasm', "MODULE_URL")) });
const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });
const hex = value => Buffer.from(value).toString('hex');
const prologue = encoder.encode('vibeface:client:v1');
// The browser's device: page scripts only ask the key to sign, like a non-extractable CryptoKey.
function device(secret) {
  const identity = Identity.fromSecretHex(secret);
  const deviceId = identity.deviceId();
  identity.free();
  const key = createPrivateKey({ key: Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), Buffer.from(secret, 'hex')]), format: 'der', type: 'pkcs8' });
  const raw = bytes => edSign(null, Buffer.from(bytes), key).toString('hex');
  return { deviceId, raw, sign: (domain, payload) => raw(signingMessage(domain, typeof payload === 'string' ? encoder.encode(payload) : payload)) };
}
async function loadSecret(file) {
  if (!file) return hex(randomBytes(32));
  try {
    const stat = await lstat(file);
    if (!stat.isFile() || stat.isSymbolicLink() || (stat.mode & 0o077)) throw new Error('client identity file must be private');
    return JSON.parse(await readFile(file, 'utf8')).secret_key_hex;
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    const secret = hex(randomBytes(32));
    await writeFile(file, JSON.stringify({ secret_key_hex: secret }) + '\n', { mode: 0o600, flag: 'wx' });
    return secret;
  }
}
const client = device(await loadSecret(process.argv[2]));
const roots = new Map();
const connections = new Map();
const invites = new Map();
const root = name => { const value = roots.get(name); if (!value) throw new Error('unknown root key'); return value; };
async function connect(input) {
  const local = input.local_cookie !== undefined;
  const url = new URL(local ? '/api/local/ws' : '/api/client', input.url.replace(/^http/, 'ws'));
  const headers = {};
  if (input.origin) headers.Origin = input.origin;
  if (local) {
    if (!input.origin || typeof input.local_cookie !== 'string' || input.local_cookie.length > 512 || /[\r\n]/.test(input.local_cookie)) throw new Error('invalid fixed local session cookie');
    headers.Cookie = input.local_cookie;
  }
  if (input.authorization !== undefined) {
    if (local || typeof input.authorization !== 'string' || !/^Bearer [A-Za-z0-9_-]{32,256}$/.test(input.authorization)) throw new Error('invalid fixed Agent authorization');
    headers.Authorization = input.authorization;
  }
  // Node's WebSocket can send the Origin a page on another client origin would send.
  const socket = new WebSocket(url, Object.keys(headers).length ? { headers } : undefined);
  socket.binaryType = 'arraybuffer';
  const queue = [], waiting = [], pending = new Map(), events = [];
  let channel, closed = false, failure, sequence = 0;
  function close(error = new Error('channel closed')) {
    if (closed) return;
    closed = true; failure = error;
    for (const waiter of waiting.splice(0)) waiter.reject(error);
    for (const request of pending.values()) { clearTimeout(request.timer); request.reject(error); }
    pending.clear(); socket.close();
  }
  const receive = () => queue.length ? Promise.resolve(queue.shift()) : closed ? Promise.reject(failure) : new Promise((resolve, reject) => waiting.push({ resolve, reject }));
  socket.addEventListener('message', event => {
    if (typeof event.data === 'string') { let reason = 'gateway refused'; try { const value = JSON.parse(event.data); reason = `${value.code}: ${value.error}`; } catch {} close(new Error(reason)); return; }
    const waiter = waiting.shift(); if (waiter) waiter.resolve(event.data); else if (queue.length < 512) queue.push(event.data); else close(new Error('bounded receive queue'));
  });
  socket.addEventListener('close', () => close());
  socket.addEventListener('error', () => close(new Error('WebSocket transport failed')));
  const timer = setTimeout(() => close(new Error('handshake timeout')), 15000);
  let handshake;
  try {
    await new Promise((resolve, reject) => { socket.addEventListener('open', resolve, { once: true }); socket.addEventListener('close', () => reject(failure), { once: true }); });
    if (!local) socket.send(JSON.stringify({ type: 'connect', target: input.device_id }));
    handshake = new Handshake(true, input.device_id, prologue);
    handshake.setProof(client.deviceId, client.raw(handshake.signingBytes()));
    socket.send(handshake.write(new Uint8Array()));
    handshake.read(new Uint8Array(await receive()));
    socket.send(handshake.write(new Uint8Array()));
    if (!handshake.isFinished() || handshake.remoteId() !== input.device_id) throw new Error('target identity mismatch');
    channel = handshake.finish();
  } catch (error) { close(error); throw error; }
  finally { clearTimeout(timer); handshake?.free(); }
  (async () => {
    try {
      while (!closed) {
        const plaintext = channel.openRecord(new Uint8Array(await receive()));
        if (plaintext === null) continue;
        const value = JSON.parse(decoder.decode(plaintext));
        const request = pending.get(value.id);
        if (request) { pending.delete(value.id); clearTimeout(request.timer); request.resolve(value); }
        else events.push(value);
      }
    } catch (error) { close(error); }
    finally { channel.free(); }
  })();
  const send = (id, method, params) => { for (const record of channel.sealMessage(encoder.encode(JSON.stringify({ id, method, params })))) socket.send(record); };
  return {
    events, close,
    request(method, params) {
      if (closed) return Promise.resolve({ error: String(failure) });
      const id = String(++sequence);
      return new Promise(resolve => {
        const timer = setTimeout(() => { pending.delete(id); resolve({ error: 'request timeout' }); }, 20000);
        pending.set(id, { resolve, reject: error => resolve({ error: String(error) }), timer });
        send(id, method, params);
      });
    },
    // The request leaves, then the connection drops before any answer: an unknown write.
    drop(method, params) { send(String(++sequence), method, params); close(new Error('dropped after send')); return { sent: true }; },
  };
}
for await (const line of createInterface({ input: process.stdin })) {
  try {
    const input = JSON.parse(line);
    let result;
    switch (input.op) {
      case 'identity': result = { device_id: client.deviceId }; break;
      case 'connect': { connections.get(input.connection || 'main')?.close(); connections.set(input.connection || 'main', await connect(input)); result = { connected: true }; break; }
      case 'genesis': {
        roots.get('root')?.free();
        const key = Identity.generate(); roots.set('root', key);
        const genesis = JSON.parse(createGenesis(key, JSON.stringify({ device_id: client.deviceId, kind: 'client', name: input.client_name, manage: true }), JSON.stringify({ device_id: input.node_id, kind: 'node', name: input.node_name, manage: false })));
        result = { genesis, network_id: key.deviceId() }; break;
      }
      case 'backup': {
        const key = root(input.key || 'root');
        const network = input.network_id || key.deviceId();
        const payload = encoder.encode(JSON.stringify({ version: 1, network_id: network, root_secret_key: key.secretHex(), genesis: input.genesis, checkpoints: input.checkpoints || [], events: input.events || [] }));
        try { result = { version: 1, network_id: network, envelope: JSON.parse(decoder.decode(recoverySeal(payload, input.password))) }; }
        finally { payload.fill(0); }
        break;
      }
      case 'forget_root': root('root').free(); roots.delete('root'); result = { forgotten: true }; break;
      case 'recover_material': {
        const plaintext = recoveryOpen(encoder.encode(JSON.stringify(input.backup.envelope)), input.password);
        try {
          const payload = JSON.parse(decoder.decode(plaintext));
          roots.get('recovered')?.free(); roots.set('recovered', Identity.fromSecretHex(payload.root_secret_key));
          result = { network_id: payload.network_id, genesis: payload.genesis, checkpoints: payload.checkpoints, events: payload.events };
        } finally { plaintext.fill(0); }
        break;
      }
      case 'recovery_read_proof': {
        const expires = Date.now() + 60000;
        const proof = `{"device_id":"${client.deviceId}","expires_at_ms":${expires},"network_id":"${input.network_id}"}`;
        result = { expires_at_ms: expires, signature: root('recovered').sign('identity-recovery-read', encoder.encode(proof)) };
        break;
      }
      case 'recovery_checkpoint': {
        const verified = verifyMembership(JSON.stringify(input.state), input.network_id);
        roots.get('next')?.free();
        const next = Identity.generate(); roots.set('next', next);
        try { result = JSON.parse(verified.createRecoveryCheckpoint(root('recovered'), next.deviceId(), JSON.stringify(input.members), JSON.stringify(input.heads))); }
        finally { verified.free(); }
        break;
      }
      case 'invite': {
        const secret = randomBytes(32);
        const record = { network_id: input.network_id, invite_hash: createHash('sha256').update(secret).digest('hex'), issuer: client.deviceId, expires_at_ms: Date.now() + 600000, signature: '' };
        record.signature = client.sign('identity-invite', inviteSigningPayload(JSON.stringify(record)));
        invites.set(record.invite_hash, secret);
        result = { record };
        break;
      }
      case 'invite_ticket': {
        const secret = invites.get(input.invite_hash);
        if (!secret) throw new Error('unknown invite');
        invites.delete(input.invite_hash);
        result = ticketEncode(input.entry_id, hex(secret), input.relay_url ?? undefined, input.direct_addrs ?? []);
        secret.fill(0);
        break;
      }
      case 'verify_address': try { verify(input.device_id, 'address', encoder.encode(input.payload), input.signature); result = true; } catch { result = false; } break;
      case 'ticket': result = JSON.parse(ticketDecode(input.ticket)); break;
      case 'commit': result = joinCommit(client.deviceId, input.nonce); break;
      case 'short_code': { const r = input.request; result = shortCode(r.network_id, r.invite_id, r.device_id, r.admin_id, r.admin_nonce, input.joiner_nonce ?? r.joiner_nonce); break; }
      case 'governance': {
        const verified = verifyMembership(JSON.stringify(input.state), input.network_id);
        try { result = JSON.parse(verified.governanceTemplate(client.deviceId, JSON.stringify(input.action))); }
        finally { verified.free(); }
        result.signature = client.sign('identity-governance', governancePayload(JSON.stringify(result)));
        break;
      }
      case 'attest': result = { device_id: client.deviceId, signature: client.sign('attestation', attestationPayload(input.network_id, input.scope_id, JSON.stringify(input.body))) }; break;
      default: {
        const connection = connections.get(input.connection || 'main');
        if (!connection) throw new Error('no connection');
        if (input.op === 'request') result = await connection.request(input.method, input.params || {});
        else if (input.op === 'drop') result = connection.drop(input.method, input.params || {});
        else if (input.op === 'events') result = connection.events;
        else throw new Error('unknown client command');
      }
    }
    process.stdout.write(`${JSON.stringify({ ok: true, result })}\n`);
  } catch (error) { process.stdout.write(`${JSON.stringify({ ok: false, error: String(error) })}\n`); }
}
for (const connection of connections.values()) connection.close();
for (const key of roots.values()) key.free();
"""


class Client:
    def __init__(self, url, helper, *, identity_file=None):
        self.url = url
        self.helper = helper
        self.identity_file = identity_file
        self.network_id = None
        self.principal = None
        self.process = None
        self.device_id = None

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

    def connect(self, device_id=None, *, connection="main", check=True, origin=None, local_cookie=None, authorization=None):
        self.device_id = device_id or self.device_id
        return self.command("connect", url=self.url, device_id=self.device_id, connection=connection, check=check,
                            **({"origin": origin} if origin else {}),
                            **({"local_cookie": local_cookie} if local_cookie is not None else {}),
                            **({"authorization": authorization} if authorization is not None else {}))

    def call(self, method, params=None, *, status=200, connection="main"):
        response = self.command("request", method=method, params=params or {}, connection=connection)
        error = response.get("error")
        if status == 200:
            require(not error, f"encrypted {method} rejected: {error}")
            return response.get("result")
        require(bool(error), f"encrypted {method} expected rejection")
        return response

    def authenticate(self, network=None):
        self.network_id = network or self.network_id
        result = self.call("authenticate", {"network_id": self.network_id})
        self.principal = result["principal"]
        require(self.principal and self.principal["network_id"] == self.network_id, "client authenticates the pinned personal network")
        return result

    def own_id(self):
        return self.command("identity")["device_id"]

    def attestation(self, method, arguments):
        """The device signature over the exact event body the Node writes for this request."""
        if method == "message":
            arguments.setdefault("report_results", True)
            body = {"type": "message_posted", "message_id": arguments["message_id"], "role": "user",
                    "text": arguments["text"], "report_results": arguments["report_results"]}
            if arguments.get("attachments"):
                arguments["attachments"] = sorted({blob["hash"]: blob for blob in arguments["attachments"]}.values(),
                                                  key=lambda blob: blob["hash"])
                body = {**{key: body[key] for key in ("type", "message_id", "role", "text")},
                        "attachments": arguments["attachments"], "report_results": body["report_results"]}
        elif method == "create_bot":
            body = {"type": "bot_created", "bot_id": arguments["bot_id"], "name": arguments["name"],
                    "instructions": arguments.get("instructions", "")}
            if arguments.get("project") is not None:
                body["project"] = arguments["project"]
        elif method == "answer":
            scope = arguments.pop("scope_id")
            body = {"type": "message_posted", "message_id": "a-" + arguments["question_id"], "role": "user",
                    "text": arguments["text"], "report_results": True, "answers": arguments["question_id"]}
            return self.command("attest", network_id=self.network_id, scope_id=scope, body=body)
        elif method == "set_primary_bot":
            body = {"type": "primary_bot_set", "conversation_id": arguments["conversation_id"]}
            return self.command("attest", network_id=self.network_id, scope_id="bots", body=body)
        elif method == "control_schedule":
            body = {"type": "schedule_controlled", "schedule_id": arguments["schedule_id"], "action": arguments["action"]}
        elif method == "set_owner":
            body = {"type": "conversation_owner", "scheduler_node": arguments["scheduler_node"], "epoch": arguments["epoch"]}
        elif method == "resolve" and arguments["check_id"].startswith("work:"):
            body = {"type": "work_finished", "work_id": arguments["check_id"].removeprefix("work:"),
                    "outcome": arguments["outcome"], "result_text": arguments.get("note", "").strip()}
        else:
            return None
        return self.command("attest", network_id=self.network_id, scope_id=arguments["conversation_id"], body=body)

    def rpc(self, method, *, status=200, unsigned=False, **arguments):
        if method in ("create_conversation", "create_bot") and not unsigned:
            arguments.setdefault("scheduler_node", self.device_id)
            arguments.setdefault("owner_attestation", self.command(
                "attest", network_id=self.network_id, scope_id=arguments["conversation_id"],
                body={"type": "conversation_owner", "scheduler_node": arguments["scheduler_node"], "epoch": 1}))
        attestation = None if unsigned else self.attestation(method, arguments)
        if attestation:
            arguments["attestation"] = attestation
        return self.call("rpc", {"method": method, **arguments}, status=status)

    def request(self, path, payload=None, *, status=200, origin=True, headers=None, timeout=20):
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

    def invite(self, *, discovery_only=False):
        """A one-time ticket link to the entry this manager is connected to."""
        made = self.command("invite", network_id=self.network_id, entry_id=self.device_id)
        tampered = copy.deepcopy(made["record"])
        tampered["expires_at_ms"] += 1
        self.call("invite", {"record": tampered}, status=400)
        entry = self.call("invite", {"record": made["record"]}).get("entry") or {}
        ticket = self.command("invite_ticket", invite_hash=made["record"]["invite_hash"], entry_id=self.device_id,
                              relay_url=None if discovery_only else entry.get("relay_url"),
                              direct_addrs=[] if discovery_only else entry.get("direct_addrs") or [])
        return f"{self.url}/join#{ticket}"

    def directory_state(self):
        return self.call("directory")["state"]

    def pending_join(self, device_id):
        requests = self.call("join_status")["requests"]
        return next((item for item in requests if item["device_id"] == device_id), None)

    def start_approval(self, join_id):
        request = self.call("approve", {"join_id": join_id, "admin_nonce": uuid.uuid4().hex + uuid.uuid4().hex})
        require(request["status"] == "confirming", "the manager's nonce moves the request to confirming")
        return request

    def grant(self, request, *, manage=False):
        member = {"device_id": request["device_id"], "kind": request["kind"], "name": request["name"],
                  "manage": manage and request["kind"] == "client"}
        event = self.command("governance", state=self.directory_state(), network_id=self.network_id,
                             action={"type": "grant", "member": member})
        result = self.call("approve", {"join_id": request["join_id"], "event": event})
        require(result["status"] == "joined", "the signed grant admits the device")
        return result

    def revoke(self, device_id, cutoff=None):
        """`cutoff` is the `(seq, hash)` of the device's last event that keeps counting."""
        seq, cutoff_hash = cutoff or (0, None)
        event = self.command("governance", state=self.directory_state(), network_id=self.network_id,
                             action={"type": "revoke", "device_id": device_id, "cutoff_seq": seq,
                                     "cutoff_hash": cutoff_hash})
        return self.call("revoke", {"event": event})

    def join(self, owner, name, *, manage=False, reveal=None, via=None):
        """Joins through a ticket from `owner` (a Node or a managing Client); approval follows matching short codes."""
        owner = getattr(owner, "client", owner)
        link = owner.invite()
        ticket = self.command("ticket", ticket=link.split("#", 1)[1])
        self.url = via or link.split("/join#", 1)[0]
        self.connect(ticket["entry_id"])
        me = self.own_id()
        nonce = uuid.uuid4().hex + uuid.uuid4().hex
        request = self.call("join", {"secret": ticket["secret"], "public_key": me, "kind": "client", "name": name,
                                     "commit": self.command("commit", nonce=nonce)})
        require(request["status"] == "pending" and request["invite_id"] == ticket["invite_id"], "the entry records the committed join")
        owner.start_approval(request["join_id"])
        status = self.call("join_status", {"join_id": request["join_id"], "reveal": reveal or nonce})
        if reveal:
            return status
        code = self.command("short_code", request=status, joiner_nonce=nonce)
        reviewed = wait_for("the manager sees the revealed nonce", lambda: (lambda item: item if item and item.get("joiner_nonce") else None)(owner.pending_join(me)))
        require(owner.command("short_code", request=reviewed) == code, "both sides show the same short code")
        owner.grant(reviewed, manage=manage)
        joined = self.call("join_status", {"join_id": request["join_id"]})
        require(joined["status"] == "joined" and joined["state"]["genesis"]["network_id"] == owner.network_id,
                "the joiner receives the signed directory")
        self.authenticate(owner.network_id)
        return joined

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
    def __init__(self, run, name, binary, workspace, helper, *, init_args=None, port=None):
        self.name = name
        self.binary = binary
        self.directory = run / name
        self.port = port if port is not None else available_port()
        require(0 < self.port < 65536, "Node listener uses a valid port")
        self.quic_port = available_port() if port is not None else self.port
        self.local_port = available_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.client = Client(self.url, helper, identity_file=self.directory / "client-identity.json")
        self.process = None
        self.log = None
        try:
            result = subprocess.run(
                [str(binary), "init", "--data-dir", str(self.directory),
                 "--listen", f"127.0.0.1:{self.port}", "--local-listen", f"127.0.0.1:{self.local_port}",
                 *(init_args if init_args is not None else ["--root", str(workspace)])],
                capture_output=True, timeout=45)
        except subprocess.TimeoutExpired:
            raise RuntimeError(f"{name}: CLI initialization timed out") from None
        require(result.returncode == 0, f"{name}: CLI initialization succeeds: {result.stderr.decode(errors='replace')[-300:]}")
        require(json.loads((self.directory / "data-epoch.json").read_text()) == {"data_epoch": DATA_EPOCH},
                "new private Node data uses the current data epoch")
        self.configure(timeout_secs=60)
        if json.loads((self.directory / "config.json").read_text())["network"]["relay_mode"] != "discovery-only":
            self.configure_network(bind=f"127.0.0.1:{self.quic_port}")
        result = subprocess.run([str(binary), "access", "--json", "--data-dir", str(self.directory)],
                                capture_output=True, timeout=15)
        require(result.returncode == 0, f"{name}: local access material is available")
        self.access = json.loads(result.stdout)
        require(self.access["device_id"], "local access pins the Node identity")
        self.device_id = self.client.device_id = self.access["device_id"]

    def configure(self, *, app_server=None, **values):
        path = self.directory / "config.json"
        configuration = json.loads(path.read_text())
        configuration.update(values)
        if app_server is not None:
            configuration["providers"]["codex_app_server"] = app_server
            configuration["providers"]["execution"] = ["codex"]
            if app_server["mode"] == "spawn":
                env_path = self.directory / "provider-env.json"
                providers = json.loads(env_path.read_text()) if env_path.exists() else {}
                providers.setdefault("codex", {})["CODEX_HOME"] = str(self.directory / "home/.codex")
                private_json(env_path, providers)
        private_json(path, configuration)

    def configure_network(self, **values):
        network = json.loads((self.directory / "config.json").read_text())["network"]
        self.configure(network={**network, **values})

    def environment(self):
        """A private HOME, so session scans and hooks never see the operator's own ~/.codex or ~/.claude."""
        home = self.directory / "home"
        (home / ".codex" / "sessions").mkdir(parents=True, exist_ok=True)
        return {**os.environ, "HOME": str(home), "CODEX_HOME": str(home / ".codex")}

    def start(self, *, env=None, no_web=False, bare=False):
        require(self.process is None, f"{self.name}: process is stopped before start")
        self.log = (self.directory / "process.log").open("ab")
        self.process = subprocess.Popen(
            [str(self.binary), *([] if bare else ["serve"]), "--data-dir", str(self.directory),
             *(["--no-web"] if no_web else [])],
            stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
            start_new_session=True, env=env or self.environment())
        if no_web or not json.loads((self.directory / "config.json").read_text())["web_enabled"]:
            health = wait_for(f"{self.name} headless maintenance socket", lambda: (
                value if (value := node_status(self))["status"] == "running" and value["web_enabled"] is False else None))
            require(health["device_id"] == self.device_id, f"{self.name}: maintenance pins its identity")
        else:
            def listener():
                if self.process.poll() is not None:
                    raise RuntimeError(f"{self.name}: Node exited {self.process.returncode}; inspect {self.directory / 'process.log'}")
                return self.client.request("/api/info")
            info = wait_for(f"{self.name} HTTP listener", listener)
            require(info["device_id"] == self.device_id, f"{self.name}: serves its own device identity")
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

    def pair(self, owner, *, discovery_only=False):
        """`vibeface join <link>` on this stopped Node; the owner approves once the short codes match."""
        require(self.process is None, f"{self.name}: joins while stopped")
        owner = getattr(owner, "client", owner)
        link = owner.invite(discovery_only=discovery_only)
        if discovery_only:
            encoded = link.split("#vibeface1", 1)[1].upper()
            ticket = base64.b32decode(encoded + "=" * (-len(encoded) % 8))
            # The browser decoder hides transport fields, so inspect this fixture's postcard entry.
            require(len(ticket) == 67 and ticket[0] == 1 and ticket[33:35] == b"\x00\x00",
                    "discovery-only CLI join ticket contains no relay or direct address")
        log_path = self.directory / f"join-cli-{uuid.uuid4().hex[:8]}.log"
        log = os.fdopen(os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb")
        process = None
        try:
            process = subprocess.Popen([str(self.binary), "join", "--data-dir", str(self.directory),
                                        "--name", f"Node {self.name}", link],
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)

            def revealed():
                if process.poll() is not None:
                    raise RuntimeError(f"{self.name}: CLI join exited {process.returncode}; see {log_path}")
                request = owner.pending_join(self.device_id)
                if request and request["status"] == "pending":
                    owner.start_approval(request["join_id"])
                    return None
                return request if request and request.get("joiner_nonce") else None
            request = wait_for(f"{self.name} reveals its join nonce", revealed)
            code = owner.command("short_code", request=request)
            shown = wait_for("the CLI shows its short code", lambda: code.encode() in log_path.read_bytes())
            require(shown, "the CLI and the manager show the same short code")
            require(request["kind"] == "node", "a CLI join asks for a Node membership")
            owner.grant(request)
            process.wait(timeout=40)
            require(process.returncode == 0, f"{self.name}: CLI join completes after the signed grant")
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            log.close()
        return code

    def bootstrap(self):
        """Creates the personal network on this fresh Node through its one-time local capability."""
        self.client.connect(self.device_id)
        info = self.client.call("info")
        require(not info["initialized"] and info["device_id"] == self.device_id, "a fresh Node reports no network")
        created = self.client.command("genesis", node_id=self.device_id, node_name=info["name"],
                                      client_name=f"{self.name} management client")
        genesis = created["genesis"]
        self.backup_password = "acceptance-only-encrypted-recovery-passphrase"
        self.backup = self.client.command("backup", genesis=genesis, password=self.backup_password)
        private_json(self.directory / "recovery-package.json", self.backup)
        self.client.command("forget_root")
        self.client.call("recover", {"genesis": genesis, "capability": "00" * 32}, status=400)
        result = self.client.call("recover", {"genesis": genesis, "capability": self.access["capability"]})
        self.client.network_id = created["network_id"]
        self.client.principal = result["principal"]
        require(self.client.principal["can_manage"], "creating the network makes this browser a manager")
        return self.client.network_id


def invocations(run, label=None):
    path = run / "fixture-invocations.jsonl"
    records = [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
    return [record for record in records if label is None or record["label"] == label]


def fixture_wrapper(run, name, mode):
    path = run / name
    path.write_text(f"#!{sys.executable}\nimport os\n" + f"os.execv({sys.executable!r}, "
                    f"[{sys.executable!r}, {str(Path(__file__).resolve())!r}, "
                    f"{mode!r}, '--fixture-root', {str(run)!r}] + __import__('sys').argv[1:])\n")
    path.chmod(0o700)
    return path


def fixture_manager_settings(run, workspace):
    """Bot turns run on the fixture App Server, so `bins.codex` must be the `--fixture-codex` wrapper."""
    return {"provider": "codex", "workspace": str(workspace), "model": "controlled-fixture"}


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
    records = sum((fixture_records(run, name) for name in ("manager-invocations.jsonl", "fixture-invocations.jsonl",
                   "codex-rpc.jsonl", "triage-invocations.jsonl", "codex-server.jsonl")), [])
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


DATA_EPOCH = int(re.search(r"^pub const DATA_EPOCH: u32 = (\d+);$",
                           (Path(__file__).resolve().parents[1] / "crates/node/src/config.rs").read_text(), re.M)[1])
# These runs drive a fixture App Server the Node starts itself, not one already on the machine.
SPAWN_APP_SERVER = {"mode": "spawn", "socket": None}
APP_SERVER_SCHEMA = Path(__file__).resolve().parents[1] / "crates/bridge/schema/app-server-methods.json"
FIXTURE_STATEFUL = {"initialize", "thread/start", "thread/resume", "thread/read", "thread/turns/list", "thread/list",
                    "thread/loaded/list", "turn/start", "turn/steer", "turn/interrupt", "thread/name/set",
                    "thread/archive", "thread/unarchive", "thread/fork", "thread/unsubscribe"}
FIXTURE_CANNED = {method: {} for method in (
    "thread/delete", "thread/goal/set", "thread/goal/clear", "thread/metadata/update", "thread/attachment/add",
    "thread/attachment/remove", "thread/section/move", "thread/compact/start", "thread/shellCommand",
    "thread/approveGuardianDeniedAction", "thread/revert", "threadSection/create", "threadSection/update",
    "threadSection/delete", "thread/inject_items", "skills/extraRoots/set", "marketplace/add", "marketplace/remove",
    "marketplace/upgrade", "plugin/reconcile", "plugin/share/save", "plugin/share/updateTargets", "plugin/share/checkout",
    "plugin/share/delete", "fs/writeFile", "fs/createDirectory", "fs/remove", "fs/copy", "fs/watch", "fs/unwatch",
    "skills/config/write", "plugin/install", "plugin/uninstall", "review/start", "account/gatewayOAuth/login",
    "account/gatewayOAuth/cancel", "experimentalFeature/enablement/set", "mcpServer/oauth/login",
    "config/mcpServer/reload", "mcpServer/resource/read", "mcpServer/tool/call", "windowsSandbox/setupStart",
    "account/login/start", "account/login/cancel", "account/logout", "account/rateLimitResetCredit/consume",
    "account/sendAddCreditsNudgeEmail", "feedback/upload", "command/exec", "command/exec/write",
    "command/exec/terminate", "command/exec/resize", "externalAgentConfig/import",
    "externalAgentConfig/import/recordHistory", "config/value/write", "config/batchWrite")}
FIXTURE_CANNED.update({
    "thread/goal/get": {"goal": None}, "thread/attachment/list": {"data": []},
    "thread/attachmentOwner/list": {"data": []}, "threadSection/list": {"data": []},
    "thread/items/list": {"data": [], "nextCursor": None}, "skills/list": {"data": []}, "hooks/list": {"data": []},
    "plugin/list": {"marketplaces": []}, "plugin/installed": {"data": []}, "plugin/read": {"plugin": None},
    "plugin/skill/read": {"skill": None}, "plugin/share/list": {"data": []}, "app/read": {"app": None},
    "app/list": {"data": [], "nextCursor": None}, "app/installed": {"data": []},
    "fs/readFile": {"dataBase64": ""}, "fs/getMetadata": {"isFile": False, "isDirectory": True},
    "fs/readDirectory": {"entries": []},
    "model/list": {"data": [{"id": "fixture-model", "model": "fixture-model", "displayName": "Fixture model",
                             "isDefault": True, "supportedReasoningEfforts": [{"reasoningEffort": "medium"}]}],
                   "nextCursor": None},
    "account/gatewayOAuth/read": {"status": "none"}, "modelProvider/capabilities/read": {"capabilities": {}},
    "experimentalFeature/list": {"data": []}, "permissionProfile/list": {"data": []},
    "mcpServerStatus/list": {"data": [], "nextCursor": None}, "windowsSandbox/readiness": {"ready": False},
    "account/rateLimits/read": {"rateLimits": {"primary": {"usedPercent": 12, "windowDurationMins": 300}}},
    "account/usage/read": {"usage": {}}, "account/workspaceMessages/read": {"data": []},
    "config/read": {"config": {}, "origins": {}}, "externalAgentConfig/detect": {"items": []},
    "externalAgentConfig/import/readHistories": {"data": []}, "configRequirements/read": {"requirements": None},
    "account/read": {"account": {"type": "apiKey"}, "requiresOpenaiAuth": False}, "fuzzyFileSearch": {"files": []},
})
# Notifications a running fixture turn emits; command text, output and diffs carry markers the UI must never show.
FIXTURE_TURN_NOTIFICATIONS = ("thread/status/changed", "turn/started", "item/started", "item/agentMessage/delta",
                              "turn/plan/updated", "item/reasoning/summaryTextDelta", "thread/tokenUsage/updated",
                              "item/commandExecution/outputDelta", "turn/diff/updated", "account/rateLimits/updated",
                              "item/completed", "turn/completed", "serverRequest/resolved", "thread/name/updated",
                              "thread/archived", "thread/unarchived", "thread/started")
FIXTURE_SECRET_MARKERS = ("fixture-secret-command", "fixture-secret-output", "fixture-secret-diff")


def fixture_schema_check():
    schema = json.loads(APP_SERVER_SCHEMA.read_text())
    handled = FIXTURE_STATEFUL | set(FIXTURE_CANNED)
    require(not FIXTURE_STATEFUL & set(FIXTURE_CANNED), "a fixture method is either stateful or canned")
    require(set(schema["client_requests"]) == handled,
            "the fake App Server answers exactly the schema's client requests: missing "
            + str(sorted(set(schema["client_requests"]) - handled)) + ", unknown " + str(sorted(handled - set(schema["client_requests"]))))
    require(set(FIXTURE_TURN_NOTIFICATIONS) <= set(schema["server_notifications"]),
            "the fake App Server emits only schema notifications")
    require({"item/commandExecution/requestApproval", "account/chatgptAuthTokens/refresh"} <= set(schema["server_requests"]),
            "the fake App Server's server requests are in the schema")
    return schema


def fixture_codex(arguments):
    """Implement the native App Server contract without loading a provider."""
    fixture_schema_check()
    run = Path(arguments[arguments.index("--fixture-root") + 1])
    address = arguments[arguments.index("--listen") + 1]
    require(address.startswith("unix://"), "fixture only binds its private Unix socket")
    socket_path = Path(address.removeprefix("unix://"))
    state_path = run / "fixture-codex-state.json"
    state = json.loads(state_path.read_text()) if state_path.exists() else {"threads": {}}
    # The Node starts this App Server after killing the previous one, as with real Codex: a turn
    # that process left in progress was interrupted with it, and its request is never asked again.
    for thread in state["threads"].values():
        if thread["turns"] and thread["turns"][-1]["status"] == "inProgress":
            thread["turns"][-1]["status"] = "interrupted"
            thread["status"] = {"type": "idle"}
            if "answer" not in (thread.get("request") or {"answer": None}):
                del thread["request"]
            private_json(state_path, state)
    state_lock = threading.Lock()
    slow_start = run / "fixture-codex-slow-start"
    if slow_start.exists():
        delay = float(slow_start.read_text())
        slow_start.unlink()
        time.sleep(delay)
    if socket_path.exists():
        socket_path.unlink()
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(socket_path))
    listener.listen(16)
    fixture_record(run, "codex-server.jsonl", {"pid": os.getpid(), "socket": str(socket_path)})

    def user_message(params):
        return {"id": "user-" + uuid.uuid4().hex, "type": "userMessage", "clientId": params.get("clientUserMessageId"),
                "content": params.get("input", [])}

    # A bot thread's Shell environment holds its tool token, so it stays in memory only, as do
    # the developer instructions it was started or resumed with.
    bot_env, bot_guide, bot_stops = {}, {}, {}

    class BotTurn:
        """One bot turn: progress and its end go to the connection that started it."""

        def __init__(self, stream, thread, turn):
            self.stream, self.thread, self.turn = stream, thread, turn
            self.stopped = bot_stops[turn["id"]] = threading.Event()

        def send(self, method, params):
            try:
                websocket_send(self.stream, {"method": method, "params": {
                    "threadId": self.thread["id"], "turnId": self.turn["id"], **params}})
            except OSError:
                pass

        def say(self, text):
            with state_lock:
                self.send("item/started", {"item": {"id": "fixture-text", "type": "agentMessage", "text": text}})

        def end(self, status, text=None):
            with state_lock:
                bot_stops.pop(self.turn["id"], None)
                if text is not None:
                    item = {"id": "fixture-answer", "type": "agentMessage", "text": text}
                    self.turn["items"].append(item)
                    self.send("item/completed", {"item": item})
                self.turn["status"] = status
                self.thread["status"] = {"type": "idle"}
                private_json(state_path, state)
                self.send("turn/completed", {"turn": self.turn})

    def bot_turn(stream, thread, turn, params, resumed):
        prompt = params["input"][0]["text"]
        guide = bot_guide.get(thread["id"], "")
        cli = re.search(r"Run:\n'(.*?)' tool TOOL_NAME", guide)
        catalog = guide[guide.find("Available tools:\n"):] if cli else ""
        history = "\n".join(part.get("text", "") for past in thread["turns"] for item in past["items"]
                            if item["type"] == "userMessage" for part in item["content"])
        initial = {"prompt": prompt, "history": history, "tool_env": bot_env.get(thread["id"], {}), "cwd": thread["cwd"],
                   "session_id": thread["id"] if resumed else None, "tool_cli": cli and cli.group(1),
                   "custom_tools": re.findall(r"(?m)^(\w+)\n[^\n]*\nJSON schema: ", catalog)}
        bot = BotTurn(stream, thread, turn)
        try:
            fixture_manager(run, initial, bot)
        except Exception as error:
            fixture_record(run, "manager-errors.jsonl", {"thread_id": thread["id"], "error": str(error)[:2000],
                                                         "trace": traceback.format_exc()[-2000:]})
            bot.end("failed")

    def summary(thread):
        return {key: value for key, value in thread.items() if key not in ("turns", "directive", "request")}

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
            connection_id, subscribed, emitted, initialized = uuid.uuid4().hex, set(), set(), False
            marker = run / "fixture-codex-disconnect"
            disconnect_seen = marker.read_text() if marker.exists() else None
            global_request = run / "fixture-codex-global-request"
            while True:
                if (marker.read_text() if marker.exists() else None) != disconnect_seen:
                    return
                with state_lock:
                    if initialized and global_request.exists() and ("global", global_request.read_text()) not in emitted:
                        emitted.add(("global", global_request.read_text()))
                        websocket_send(stream, {"id": "global-" + global_request.read_text(),
                                                "method": "account/chatgptAuthTokens/refresh", "params": {"reason": "unauthorized"}})
                    for thread_id in list(subscribed):
                        thread = state["threads"].get(thread_id)
                        if not thread or not thread["turns"] or thread.get("bot"):
                            continue
                        turn = thread["turns"][-1]
                        directive = thread.get("directive", {})
                        gate = directive.get("gate")
                        request = thread.get("request")
                        if turn["status"] == "inProgress" and gate and Path(gate).exists() and (
                                not request or "answer" in request):
                            thread["status"] = {"type": "idle"}
                            turn["status"] = "completed"
                            turn["items"].append({"id": "fixture-answer", "type": "agentMessage",
                                                  "text": "fixture Codex terminal result"})
                            private_json(state_path, state)
                        identity = (thread_id, turn["id"])
                        if turn["status"] == "inProgress" and identity not in emitted:
                            emitted.add(identity)
                            for method, params in (
                                    ("thread/status/changed", {"threadId": thread_id, "status": thread["status"]}),
                                    ("turn/started", {"threadId": thread_id, "turn": {"id": turn["id"], "status": "inProgress", "items": []}}),
                                    ("item/agentMessage/delta", {"threadId": thread_id, "turnId": turn["id"], "itemId": "fixture-answer", "delta": "native working"}),
                                    ("turn/plan/updated", {"threadId": thread_id, "turnId": turn["id"], "explanation": "fixture plan",
                                                           "plan": [{"step": "读代码", "status": "completed"}, {"step": "改代码", "status": "inProgress"}]}),
                                    ("item/reasoning/summaryTextDelta", {"threadId": thread_id, "turnId": turn["id"], "itemId": "fixture-reasoning",
                                                                         "summaryIndex": 0, "delta": "fixture reasoning summary"}),
                                    ("thread/tokenUsage/updated", {"threadId": thread_id, "turnId": turn["id"], "tokenUsage": {
                                        "total": {"totalTokens": 1234, "inputTokens": 1000, "outputTokens": 234}, "modelContextWindow": 200000}}),
                                    ("item/started", {"threadId": thread_id, "turnId": turn["id"], "item": {
                                        "id": "fixture-command", "type": "commandExecution", "command": "printf fixture-secret-command",
                                        "cwd": thread["cwd"], "status": "inProgress"}}),
                                    ("item/commandExecution/outputDelta", {"threadId": thread_id, "turnId": turn["id"],
                                                                           "itemId": "fixture-command", "delta": "fixture-secret-output"}),
                                    ("turn/diff/updated", {"threadId": thread_id, "turnId": turn["id"], "diff": "+fixture-secret-diff"}),
                                    ("account/rateLimits/updated", {"rateLimits": {"primary": {"usedPercent": 12, "windowDurationMins": 300}}})):
                                websocket_send(stream, {"method": method, "params": params})
                        if request and "answer" not in request and turn["status"] == "inProgress" and (thread_id, "request") not in emitted:
                            emitted.add((thread_id, "request"))
                            websocket_send(stream, {"id": request["id"], "method": request["method"], "params": {
                                "threadId": thread_id, "turnId": turn["id"], "itemId": "fixture-command",
                                "command": "printf fixture-secret-command", "cwd": thread["cwd"],
                                "availableDecisions": ["accept", "decline", "cancel"], "reason": "controlled request acceptance"}})
                            fixture_record(run, "codex-events.jsonl", {"connection": connection_id, "thread_id": thread_id, "event": "request"})
                        if turn["status"] == "inProgress" and directive.get("fail") and identity in emitted:
                            thread["status"] = {"type": "idle"}
                            turn["status"] = "failed"
                            turn["error"] = {"message": directive["fail"]}
                            private_json(state_path, state)
                        terminal = (*identity, turn["status"])
                        if turn["status"] != "inProgress" and terminal not in emitted:
                            emitted.add(terminal)
                            for item in turn["items"]:
                                if item["type"] == "agentMessage":
                                    websocket_send(stream, {"method": "item/completed", "params": {
                                        "threadId": thread_id, "turnId": turn["id"], "item": item}})
                            event = {"method": "turn/completed", "params": {"threadId": thread_id, "turn": turn}}
                            websocket_send(stream, event)
                            websocket_send(stream, event)
                if not select.select([stream], [], [], .1)[0]:
                    continue
                message = websocket_receive(stream)
                method, params = message.get("method"), message.get("params", {})
                bot = bool(state["threads"].get(params.get("threadId"), {}).get("bot")) or params.get("threadSource") == "vibeface-bot"
                fixture_record(run, "codex-rpc.jsonl", {"connection": connection_id, "pid": os.getpid(), "message": message,
                                                        "bot": bot})
                initialized = initialized or method == "initialized"
                with state_lock:
                    if method is None:
                        answered = str(message.get("id", ""))
                        for thread in state["threads"].values():
                            request = thread.get("request")
                            if request and request["id"] == answered and "answer" not in request:
                                request["answer"] = message.get("result", {"error": message.get("error")})
                                private_json(state_path, state)
                                websocket_send(stream, {"method": "serverRequest/resolved", "params": {
                                    "threadId": thread["id"], "requestId": answered}})
                        continue
                    if "id" not in message:
                        continue
                    error = None
                    thread_id = params.get("threadId")
                    thread = state["threads"].get(thread_id) if thread_id else None
                    if method in FIXTURE_CANNED:
                        result = copy.deepcopy(FIXTURE_CANNED[method])
                    elif method == "initialize":
                        result = {"userAgent": "controlled-fixture", "platformFamily": "unix", "platformOs": sys.platform}
                    elif method == "thread/loaded/list":
                        result = {"data": list(state["threads"]), "nextCursor": None}
                    elif method == "thread/list":
                        result = {"data": [summary(row) for row in state["threads"].values()
                                           if not row.get("archived") or params.get("archived")], "nextCursor": None}
                    elif method == "thread/start":
                        thread_id = "fixture-thread-" + uuid.uuid4().hex
                        thread = {"id": thread_id, "cwd": params["cwd"], "status": {"type": "idle"}, "turns": [],
                                  "name": None, "preview": "", "createdAt": int(time.time())}
                        if params.get("threadSource") == "vibeface-bot":
                            thread["bot"] = True
                            bot_env[thread_id] = params["config"]["shell_environment_policy.set"]
                            bot_guide[thread_id] = params.get("developerInstructions") or ""
                        state["threads"][thread_id] = thread
                        result = {"thread": thread}
                        subscribed.add(thread_id)
                        websocket_send(stream, {"method": "thread/started", "params": {"thread": summary(thread)}})
                    elif method not in FIXTURE_STATEFUL:
                        error = {"code": -32601, "message": "fixture does not implement " + method}
                    elif thread is None:
                        error = {"code": -32600, "message": "thread not found: " + str(thread_id)}
                    elif method == "thread/resume" and not thread["turns"]:
                        error = {"code": -32600, "message": "no rollout found for thread id " + thread_id}
                    elif method == "turn/start" and thread.get("bot"):
                        if thread["status"]["type"] == "active":
                            error = {"code": -32600, "message": "a turn is already running"}
                        else:
                            resumed = bool(thread["turns"])
                            thread["status"] = {"type": "active", "activeFlags": []}
                            turn = {"id": "fixture-turn-" + uuid.uuid4().hex, "status": "inProgress", "items": [user_message(params)]}
                            thread["turns"].append(turn)
                            result = {"turn": turn}
                            threading.Thread(target=bot_turn, args=(stream, thread, turn, params, resumed), daemon=True).start()
                    elif method == "turn/start":
                        if thread["status"]["type"] == "active":
                            error = {"code": -32600, "message": "a turn is already running"}
                        else:
                            try:
                                directive = json.loads(params["input"][0]["text"])
                            except (ValueError, KeyError, IndexError, TypeError):
                                directive = {}
                            thread["directive"] = directive if isinstance(directive, dict) else {}
                            if thread["directive"].get("request"):
                                thread["request"] = {"id": "req-" + uuid.uuid4().hex[:12], "method": thread["directive"]["request"]}
                            else:
                                thread.pop("request", None)
                            thread["status"] = {"type": "active", "activeFlags": []}
                            turn = {"id": "fixture-turn-" + uuid.uuid4().hex, "status": "inProgress", "items": [user_message(params)]}
                            thread["turns"].append(turn)
                            thread["preview"] = thread["preview"] or str(params["input"][0].get("text", ""))[:80]
                            result = {"turn": turn}
                            if thread["directive"].get("drop_reply"):
                                private_json(state_path, state)
                                return
                    elif method == "turn/steer":
                        active = next((turn for turn in reversed(thread["turns"]) if turn["status"] == "inProgress"), None)
                        if not active or active["id"] != params.get("expectedTurnId"):
                            error = {"code": -32600, "message": "expectedTurnId does not match the active turn"}
                        else:
                            active["items"].append(user_message(params))
                            result = {"turnId": active["id"]}
                    elif method == "turn/interrupt" and params.get("turnId") in bot_stops:
                        bot_stops[params["turnId"]].set()
                        result = {}
                    elif method == "turn/interrupt":
                        require(thread["turns"][-1]["id"] == params["turnId"], "interrupt selects the original turn")
                        thread["status"] = {"type": "idle"}
                        thread["turns"][-1]["status"] = "interrupted"
                        result = {}
                    elif method == "thread/turns/list":
                        turns = thread["turns"][::-1] if params.get("sortDirection") == "desc" else thread["turns"]
                        result = {"data": turns[:params.get("limit") or None], "nextCursor": None}
                    elif method == "thread/name/set":
                        thread["name"] = params.get("name")
                        result = {}
                        websocket_send(stream, {"method": "thread/name/updated", "params": {"threadId": thread_id, "threadName": thread["name"]}})
                    elif method in ("thread/archive", "thread/unarchive"):
                        thread["archived"] = method == "thread/archive"
                        result = {}
                        websocket_send(stream, {"method": method.replace("/archive", "/archived").replace("/unarchive", "/unarchived"),
                                                "params": {"threadId": thread_id}})
                    elif method == "thread/fork":
                        fork = {**copy.deepcopy(thread), "id": "fixture-thread-" + uuid.uuid4().hex, "status": {"type": "idle"}}
                        fork.pop("directive", None)
                        fork.pop("request", None)
                        state["threads"][fork["id"]] = fork
                        result = {"thread": fork}
                    elif method == "thread/unsubscribe":
                        subscribed.discard(thread_id)
                        result = {"status": "unsubscribed"}
                    else:
                        result = {"thread": thread}
                        if method == "thread/resume":
                            subscribed.add(thread_id)
                            if thread.get("bot"):
                                bot_env[thread_id] = params["config"]["shell_environment_policy.set"]
                                bot_guide[thread_id] = params.get("developerInstructions") or ""
                    if error:
                        websocket_send(stream, {"id": message["id"], "error": error})
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


READ_TOOLS = {"nodes", "context", "work_status", "call_lookup", "read_skill", "recall", "fetch_url", "list_bots", "read_bot", "list_sessions"}
WORK_TOOLS = {"dispatch_work", "interrupt_work", "hand_over_session", "propose_provider_config", "apply_provider_config"}
WAKE_TOOLS = {"remind", "message_bot"}
SETTINGS_TOOLS = {"update_profile", "save_skill", "remove_skill", "save_schedule", "remove_schedule", "remember", "forget",
                  "create_bot", "configure_bot", "delete_bot"}
TRIAGE_WAKE = "New events from the user's local coding agent sessions"


def fixture_cli(initial, *arguments, check=True):
    """Runs the real `vibeface` CLI the way an Agent's Shell would, with the bot turn's tool environment."""
    completed = subprocess.run([initial["tool_cli"], *arguments], capture_output=True, text=True, timeout=60,
                               env={**os.environ, **initial["tool_env"]})
    require(not check or completed.returncode == 0,
            "fixture CLI " + " ".join(arguments[:2]) + " succeeds: " + completed.stderr[-500:])
    return completed


def fixture_triage(run, initial, prompt, bot):
    """The coordination Agent's triage turn: answer, ask about or dismiss each event per the fixture thread's directive."""
    while (run / "hold-triage").exists():
        time.sleep(0.05)
    events = json.loads(prompt[prompt.index("Events:\n") + len("Events:\n"):])
    state_path = run / "fixture-codex-state.json"
    threads = json.loads(state_path.read_text())["threads"] if state_path.exists() else {}
    for event in events:
        directive = threads.get(event["session_id"], {}).get("directive", {})
        action = {"event_id": event["event_id"], "event": event["event"], "session_id": event["session_id"],
                  "request_id": event.get("request_id"), "cwd": event["cwd"], "pid": os.getpid()}
        mode = directive.get("request_mode", "respond")
        if event.get("request_id"):
            listed = json.loads(fixture_cli(initial, "requests").stdout)["requests"]
            require(any(row["request_id"] == event["request_id"] for row in listed),
                    "requests lists the held App Server request")
            if mode == "respond":
                answers = [json.loads(fixture_cli(initial, "respond", event["request_id"],
                                                  json.dumps({"decision": "accept"})).stdout) for _ in range(2)]
                action.update(action="respond", answers=answers)
            elif mode == "ask":
                asked = [json.loads(fixture_cli(initial, "triage", "raise", "--event-id", event["event_id"], "--severity", "urgent",
                                                "--question", "Codex 想运行一条命令，允许吗？", "--request-id", event["request_id"],
                                                "--choice", "允许", "--choice", "拒绝").stdout) for _ in range(2)]
                action.update(action="ask", asked=asked)
            else:
                action.update(action="ignore")
        elif "triage-raise" in event["cwd"] or "needs-human" in event.get("last_message", ""):
            choices = ["--choice", "继续", "--choice", "停下"] if "choose-next" in event.get("last_message", "") else []
            asked = json.loads(fixture_cli(initial, "triage", "raise", "--event-id", event["event_id"], "--severity", "attention",
                                           "--question", "会话停下等你回复：下一步做什么？", *choices).stdout)
            action.update(action="raise", asked=asked)
        else:
            fixture_cli(initial, "triage", "dismiss", "--event-id", event["event_id"])
            action.update(action="dismiss")
        fixture_record(run, "triage-actions.jsonl", action)
    bot.end("completed", "已处理")


def fixture_manager(run, initial, bot):
    """A bot turn on the fake App Server: the real `vibeface` CLIs and scoped Node tools, never a model."""
    prompt = initial["prompt"]
    require("VIBEFACE_CODEX_SOCKET" in initial["tool_env"], "the coordination Agent's Shell reaches the App Server socket")
    native = bot.thread["id"]
    if TRIAGE_WAKE in prompt:
        fixture_record(run, "triage-invocations.jsonl", {"pid": os.getpid(), "native_session_id": native,
                                                        "attempt": initial["tool_env"].get("VIBEFACE_TRIAGE_ATTEMPT")})
        initial["session_id"] = native
        fixture_triage(run, initial, prompt, bot)
        return
    start = prompt.rfind("\n\n{")
    require(start >= 0, "fixture receives explicit management context")
    context, _ = json.JSONDecoder().raw_decode(prompt[start + 2:])
    trigger, kind = context["trigger"]["trigger"], context["trigger"]["kind"]
    body = (context["trigger"].get("event") or {}).get("body", {})
    autonomous = kind == "work_result"
    directive = {}
    if not autonomous:
        try:
            candidate = json.loads(body.get("prompt" if kind == "agent_task" else "text", ""))
            if isinstance(candidate, dict):
                directive = candidate
        except ValueError:
            pass
    label = "result-" + body.get("work_id", "") if autonomous else directive.get("label", body.get("message_id", ""))
    record = {"conversation_id": context["conversation_id"], "trigger": trigger, "label": label,
              "native_session_id": native, "resumed": bool(initial.get("session_id")),
              "autonomous": autonomous, "pid": os.getpid(), "kind": kind,
              "work_id": body.get("work_id"), "outcome": body.get("outcome"),
              "resumed_session": context["conversation"]["resumed_session"],
              "checkpoint": context["conversation"]["checkpoint"] is not None,
              "messages": [row["text"][:200] for row in context["conversation"]["messages"]],
              "trigger_text": str(((context["trigger"].get("event") or {}).get("body") or {}).get("text", ""))[:200],
              "works": [row["work_id"] for row in context["works"]] if isinstance(context["works"], list) else None,
              "bot": context["bot"].get("configured_bot"),
              "sessions": [row["session_id"] for row in context["bot"].get("sessions") or []],
              "bot_prompt_markers": [marker for marker in
                  ("bot-alpha-instructions-sentinel", "bot-beta-instructions-sentinel") if marker in initial["history"]],
              "node_url": initial["tool_env"]["VIBEFACE_NODE_URL"]}
    fixture_record(run, "manager-invocations.jsonl", record)
    # A held report keeps a failed task's result from being answered, so it waits on the person.
    if autonomous and (run / "fixture-hold-report").exists():
        bot.end("failed")
        return
    initial_tools = {"tool_search", "tool_describe", "tool_call", "context", "read_skill", "call_lookup"}
    require(set(initial["custom_tools"]) == initial_tools,
            "the provider initially receives discovery and the required history/skill/read-back tools")
    expected_tools = READ_TOOLS | WORK_TOOLS
    if kind != "agent_task":
        expected_tools |= WAKE_TOOLS
    if kind == "human":
        expected_tools |= SETTINGS_TOOLS
    described = set()

    def tool(name, arguments, expected=True, rejection="unknown", discard=False):
        if expected and name not in initial_tools and name not in described:
            schema = tool("tool_describe", {"tool": name})
            require(schema["name"] == name and schema["input_schema"]["type"] == "object"
                    and schema["input_schema"]["additionalProperties"] is False,
                    "tool discovery returns the original domain schema")
            described.add(name)
        target, payload = (name, arguments) if name in initial_tools else ("tool_call", {"tool": name, "arguments": arguments})
        environment = initial["tool_env"]
        request = urllib.request.Request(
            environment["VIBEFACE_NODE_URL"] + "/sdk.v1.SdkCustomToolCallbackService/CallCustomTool",
            json.dumps({"toolName": target, "args": payload, "agentId": "fixture"}).encode(),
            {"Content-Type": "application/json", "Authorization": "Bearer " + environment["VIBEFACE_TOOL_TOKEN"]})
        started = time.monotonic()
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                value = None if discard else json.load(response)["result"]
            accepted = True
        except urllib.error.HTTPError as error:
            raw_error = error.read(16385)
            constraint = ""
            try:
                payload = json.loads(raw_error)
                constraint = payload.get("error", "") if isinstance(payload, dict) else ""
            except (ValueError, UnicodeDecodeError):
                pass
            constraint = constraint if isinstance(constraint, str) else ""
            fixture_record(run, "manager-tool-errors.jsonl", {
                "conversation_id": context["conversation_id"], "trigger": trigger, "name": name,
                "http_status": error.code, "error": constraint.replace(environment["VIBEFACE_TOOL_TOKEN"], "[redacted]")[:2048]})
            require(error.code == 400 and rejection in constraint,
                    "negative tool check rejects specifically for its expected authority constraint")
            value, accepted = None, False
        except (OSError, http.client.HTTPException) as error:
            raise RuntimeError("fixture tool transport failed with " + type(error).__name__) from None
        fixture_record(run, "manager-tools.jsonl", {"conversation_id": context["conversation_id"],
                       "trigger": trigger, "name": name, "accepted": accepted, "expected": expected,
                       "call_ref": arguments.get("call_ref"), "response_discarded": discard,
                       "result": None if discard else value,
                       "elapsed_ms": round((time.monotonic() - started) * 1000)})
        require(accepted == expected, "scoped tool " + name + " has its expected authority")
        return None if discard else value

    catalog, offset = set(), 0
    while True:
        page = tool("tool_search", {"offset": offset, "limit": 30})
        names = [row["name"] for row in page["tools"]]
        require(not (catalog & set(names)), "tool discovery pages do not repeat entries")
        catalog.update(names)
        if not page["more"]:
            break
        require(isinstance(page["next"], int) and page["next"] > offset,
                "tool discovery has a forward-only bounded cursor")
        offset = page["next"]
    require(catalog == expected_tools, "dynamic discovery exposes exactly this turn's authorized domain tools")

    def dispatch_cli(arguments, expected=True, rejection=""):
        """`vibeface tool dispatch_work` from the Agent's Shell: the CLI registers, then talks to the socket itself."""
        completed = fixture_cli(initial, "tool", "dispatch_work", "--json", json.dumps(arguments),
                                "--call-ref", arguments["call_ref"], check=False)
        accepted = completed.returncode == 0
        fixture_record(run, "manager-tools.jsonl", {"conversation_id": context["conversation_id"], "trigger": trigger,
                       "name": "dispatch_work", "cli": True, "accepted": accepted, "expected": expected,
                       "call_ref": arguments["call_ref"], "result": json.loads(completed.stdout) if accepted else None,
                       "error": completed.stderr[-2048:]})
        require(accepted == expected and (accepted or rejection in completed.stderr),
                "dispatch_work from the Shell has its expected outcome: " + completed.stderr[-300:])
        return json.loads(completed.stdout) if accepted else None

    if directive.get("paid_delivery_security") is True:
        fixture_paid_principals(run, initial, bot, tool)
        return

    if kind == "answer":
        question, answer = context["trigger"]["question"], body["text"]
        about = question["about"]
        if about["agent"] is None:
            replies = [tool("apply_provider_config", {"question_id": question["question_id"]}) for _ in range(2)]
            require(replies[0] == replies[1] and replies[0]["status"] in ("applied", "declined"),
                    "a settings card's answer is applied once; a repeat reads it back")
        elif about.get("request_id"):
            result = json.dumps({"decision": "accept" if answer == "允许" else "decline"})
            replies = [json.loads(fixture_cli(initial, "respond", about["request_id"], result, "--node", about["node"]).stdout)
                       for _ in range(2)]
        else:
            replies = [json.loads(fixture_cli(initial, "codex", "send", about["session_id"], answer, "--id", body["message_id"]).stdout)
                       for _ in range(2)]
        fixture_record(run, "answer-actions.jsonl", {"conversation_id": context["conversation_id"], "trigger": trigger,
                       "question_id": question["question_id"], "answer": answer, "replies": replies, "pid": os.getpid()})
    host_command = directive.get("host_command")
    if host_command:
        fixture_record(run, "fixture-invocations.jsonl", {"native_session_id": native,
                       "label": label, "pid": os.getpid(), "trigger": trigger})
        completed = subprocess.run(["/bin/sh", "-c", host_command],
                                   cwd=directive.get("host_cwd", initial["cwd"]),
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=45)
        require(completed.returncode == 0, "fixture host command exits successfully")
    if autonomous:
        tool("update_profile", {"call_ref": "forbidden-rename", "name": "forbidden"}, expected=False,
             rejection="is not offered in")
        status = tool("work_status", {"work_id": body["work_id"]})
        fixture_record(run, "manager-results.jsonl", {"trigger": trigger, "work_id": body["work_id"],
                       "outcome": body.get("outcome"), "status_outcome": status["outcome"]})
    dispatch = directive.get("dispatch")
    if dispatch:
        arguments = {"title": dispatch.get("title", dispatch["label"]), "prompt": json.dumps(dispatch["native"]), "cwd": dispatch["cwd"], "call_ref": "work-" + dispatch["label"],
                     **{key: dispatch[key] for key in ("parent", "target_node", "continues", "model", "effort") if key in dispatch}}
        work = None
        for index in range(dispatch.get("repeat", 1)):
            work = dispatch_cli(arguments)
            if dispatch.get("discard_first") and index == 0:
                work = None
        found = tool("call_lookup", {"call_ref": arguments["call_ref"]})
        require(work is None or work["work_id"] == found["work_id"], "call_lookup finds the dispatched work")
        if dispatch.get("changed"):
            dispatch_cli({**arguments, "prompt": "different work"}, expected=False, rejection=dispatch["changed"])
        fixture_record(run, "manager-native-work.jsonl", {"trigger": trigger, "label": dispatch["label"],
                       "work_id": found["work_id"], "first": work})
    send = directive.get("send")
    if send:
        command = ("codex", "send", send["thread"], send["text"], "--id", send["id"])
        sent = [json.loads(fixture_cli(initial, *command).stdout) for _ in range(send.get("repeat", 1))]
        conflict = fixture_cli(initial, *command[:3], send["conflict_text"], *command[4:], check=False) \
            if send.get("conflict_text") else None
        fixture_record(run, "manager-sends.jsonl", {"trigger": trigger, "label": label, "sent": sent,
                       "conflict": None if conflict is None else {"code": conflict.returncode, "error": conflict.stderr[-500:]}})
    if kind in ("schedule", "bot_message"):
        tool("update_profile", {"call_ref": kind + "-rename", "name": "forbidden"}, expected=False,
             rejection="is not offered in")
    rename = re.match(r"^(rename yourself to|改名为|改名叫)\s*(.+)$", body.get("text", "")) if kind == "human" else None
    if rename:
        tool("update_profile", {"call_ref": "rename", "name": rename.group(2).strip()})
    for action in directive.get("tools", []):
        tool(action["name"], action["arguments"], expected=action.get("accepted", True),
             rejection=action.get("rejection", "unknown"))
    sequence = 0
    gate = directive.get("gate")
    # Screens show these replies, so only an explicit test label is echoed back.
    explicit = directive.get("label")
    spoken = "已看过任务结果" if autonomous else "收到，已处理"
    if kind == "answer" and question["about"]["agent"] is None:
        spoken = "设置已更新" if replies[0]["status"] == "applied" else "没有改动设置"
    while True:
        sequence += 1
        bot.say(f"{explicit}:{sequence}" if explicit else spoken)
        if not gate or Path(gate).exists():
            break
        if bot.stopped.wait(0.2):
            if directive.get("cancel_unknown"):
                time.sleep(10)
            bot.end("interrupted")
            return
    steered = [item["clientId"] for item in bot.turn["items"][1:] if item["type"] == "userMessage"]
    final = "x" * directive.get("result_size", 0) or (f"{explicit}:complete" if explicit else spoken + "。")
    bot.end("completed", final + "".join(" +" + message for message in steered))


def fixture_paid_principals(run, initial, bot, tool):
    specification = run / "paid-principal-input.json"
    require(stat.S_ISREG(specification.lstat().st_mode) and specification.stat().st_size <= 4096
            and specification.stat().st_mode & 0o077 == 0, "the paid principal turn reads one private bounded fixture input")
    value = json.loads(specification.read_text())
    require(set(value) == {"source_url", "source_peer", "network", "order_id", "intent_id", "configure", "helper", "outsider_identity", "outsider_peer"},
            "the live Agent check has one fixed paid principal schema")
    for name in ("source_peer", "network", "order_id", "outsider_peer"):
        require(re.fullmatch(r"[a-f0-9]{64}", value[name]), "the live Agent pins each actual peer, network and original order")
    require(str(uuid.UUID(value["intent_id"])) == value["intent_id"]
            and set(value["configure"]) == {"enabled", "charity_enabled", "paid_enabled"},
            "the live Agent uses the original UUID and the existing settings")
    base = Path(__file__).resolve().parents[1] / ".artifacts"
    require(run.resolve().is_relative_to(base) and Path(value["helper"]).resolve().is_relative_to(base)
            and Path(value["outsider_identity"]).resolve().is_relative_to(base), "Agent principal materials belong only to the task fixture")
    parsed = urllib.parse.urlsplit(value["source_url"])
    require(parsed.scheme == "http" and parsed.hostname == "127.0.0.1" and parsed.port and parsed.path in ("", "/")
            and not parsed.query and not parsed.fragment, "Agent principal checks use only the actual loopback Node")
    private_json(run / "agent-ready.json", {"ready": True})
    wait_for("the original paid principal baseline releases its live Agent", lambda:
             (run / "agent-run.json").exists() and not bot.stopped.is_set(), timeout=180)
    outsider = Client(value["source_url"], Path(value["helper"]), identity_file=Path(value["outsider_identity"]))
    checks = []
    try:
        positive = tool("nodes", {})
        require(isinstance(positive, dict) and positive.get("nodes"), "the actual live scoped Agent token reads its Node catalog")
        requests = ({"op": "confirm", "intent_id": value["intent_id"]},
                    {"op": "configure", **value["configure"]}, {"op": "get", "order_id": value["order_id"]})
        for request in requests:
            tool("delivery", request, expected=False, rejection="unknown or unavailable coordination tool")
            checks.append({"entry": "agent-tool", "op": request["op"], "rejection": "unknown or unavailable coordination tool"})
        require(outsider.own_id() == value["outsider_peer"], "the Agent bearer probe retains the actual unjoined X identity")
        outsider.connect(value["source_peer"], origin=value["source_url"],
                         authorization="Bearer " + initial["tool_env"]["VIBEFACE_TOOL_TOKEN"])
        require(outsider.call("authenticate", {"network_id": value["network"]})["principal"] is None,
                "a valid live Agent bearer cannot become a client membership principal")
        for request in requests:
            error = outsider.call("delivery", request, status=400).get("error")
            require(isinstance(error, str) and "this device is not a member of the personal network" in error,
                    "the live Agent bearer leaves each encrypted client action outside the personal network")
            checks.append({"entry": "agent-bearer-client", "op": request["op"], "rejection": "this device is not a member of the personal network"})
        error = outsider.rpc("nodes", status=400).get("error")
        require(isinstance(error, str) and "this device is not a member of the personal network" in error,
                "a valid Agent bearer also grants no private client Node directory")
        checks.append({"entry": "agent-bearer-client", "op": "nodes", "rejection": "this device is not a member of the personal network"})
        require(tool("nodes", {}).get("nodes"), "the same real Agent token remains valid after every boundary rejection")
        private_json(run / "agent-checked.json", {"passed": True, "valid_token_before_and_after": True, "checks": checks})
        wait_for("the original paid principal observation releases its normal Agent completion", lambda:
                 (run / "agent-finish.json").exists() and not bot.stopped.is_set(), timeout=180)
        bot.end("completed", "paid-principal-security-complete")
    finally:
        outsider.close()


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

    worker, worker_headers = fetch("/sw.js")
    require(worker_headers.get_content_type() == "text/javascript" and worker_headers["Cache-Control"] == "no-cache"
            and b"/api/" in worker, "the offline service worker is served as revalidated JavaScript and skips the API")
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
        if path in ("/manifest.webmanifest", "/icon.svg"):
            require(asset_headers["Cache-Control"] == "no-cache", f"{path}: unhashed app-shell files revalidate")
        else:
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
    for path in ("/api", "/api/not-a-route", "/sdk.v1.SdkCustomToolCallbackService/CallCustomTool",
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


def recover_fresh_node(node, backup, password):
    """A fresh Node takes the recovery through its local capability, and it waits like any other."""
    node.start()
    node.client.connect(node.device_id)
    material = node.client.command("recover_material", backup=backup, password=password)
    info = node.client.call("info")
    members = [{"device_id": node.client.own_id(), "kind": "client", "name": "Fresh recovery manager", "manage": True},
               {"device_id": node.device_id, "kind": "node", "name": info["name"], "manage": False}]
    state = {"genesis": material["genesis"], "checkpoints": material["checkpoints"], "events": material["events"]}
    checkpoint = node.client.command("recovery_checkpoint", state=state, network_id=material["network_id"],
                                     members=members, heads={})
    submit = {"genesis": material["genesis"], "checkpoints": [*material["checkpoints"], checkpoint],
              "events": material["events"]}
    node.client.call("recover", submit, status=400)
    result = node.client.call("recover", {**submit, "capability": node.access["capability"]})
    node.client.network_id = backup["network_id"]
    require(result["principal"] is None and result["recovery"]["status"] == "waiting",
            "a recovery on a fresh Node waits like any other")
    node.client.call("rpc", {"method": "conversations"}, status=400)
    return checkpoint


def verify_recovery(node, other, network):
    """A browser holding only the recovery file must wait; a manager stops it and the old file keeps working."""
    wrong = other.command("recover_material", backup=node.backup, password="wrong-acceptance-passphrase", check=False)
    require(wrong["ok"] is False, "encrypted recovery package rejects a wrong password")
    material = other.command("recover_material", backup=node.backup, password=node.backup_password)
    require(material["network_id"] == network, "recovered root retains the original personal fingerprint")
    other.url = node.url
    other.connect(node.device_id)
    me = other.own_id()
    stale = other.command("recovery_read_proof", network_id=network)
    stale["signature"] = node.client.command("attest", network_id=network, scope_id="x",
                                             body={"type": "probe"})["signature"]
    response = other.call("recovery_state", stale, status=400)
    require("state" not in response, "a proof not signed by the root reveals no membership")
    def read_with(checkpoints, events, status=200):
        proof = other.command("recovery_read_proof", network_id=network)
        return other.call("recovery_state", {**proof, "checkpoints": checkpoints, "events": events}, status=status)

    read = read_with(material["checkpoints"], material["events"])
    view = node.client.call("directory")["view"]
    revoked = {item["device_id"] for item in view["revoked"]}
    keep = [member for member in view["members"] if member["device_id"] not in revoked]
    managers = [member["device_id"] for member in keep if member["manage"]]
    keep.append({"device_id": me, "kind": "client", "name": "Recovered management client", "manage": True})
    checkpoint = other.command("recovery_checkpoint", state=read["state"], network_id=network, members=keep,
                               heads=read["heads"])
    chain, events = [*read["state"]["checkpoints"], checkpoint], read["state"]["events"]
    renewed = other.command("backup", key="next", network_id=network, genesis=material["genesis"], checkpoints=chain,
                            events=events, password=node.backup_password)
    submit = {"genesis": material["genesis"], "checkpoints": chain, "events": events}
    result = other.call("recover", submit)
    other.network_id = network
    waiting = result["recovery"]
    require(result["principal"] is None and waiting["status"] == "waiting"
            and waiting["effective_at_ms"] == checkpoint["issued_at_ms"] + 48 * 3600 * 1000,
            "a root recovery waits two days before it takes effect")
    require(other.call("recover", submit)["recovery"] == waiting, "retrying the same recovery changes nothing")
    other.call("directory", status=400)
    directory = node.client.call("directory")
    pending = directory["view"]["pending"]
    require(pending["effective_at_ms"] == waiting["effective_at_ms"] and directory["view"]["epoch"] == view["epoch"],
            "managers see the waiting recovery and keep the directory in effect")
    refused = read_with([], [], status=400)
    require("state" not in refused and "another recovery is waiting" in json.dumps(refused),
            "the old recovery file signs nothing while a recovery waits")
    veto = node.client.command("governance", state=directory["state"], network_id=network,
                               action={"type": "veto", "checkpoint": pending["checkpoint"], "at_ms": directory["now_ms"]})
    other.call("veto", {"event": veto}, status=400)
    stopped = node.client.call("veto", {"event": veto})
    require(stopped["view"]["pending"] is None and stopped["view"]["stopped"] == [pending["checkpoint"]],
            "an existing manager stops the recovery")
    require(node.client.call("veto", {"event": veto})["view"] == stopped["view"], "retrying the veto changes nothing")
    require(other.call("authenticate")["recovery"] == {"status": "stopped"}, "the recovering browser learns it was stopped")
    read_with([], [])
    other.command("recover_material", backup=renewed, password=node.backup_password)
    refused = read_with(chain, events, status=400)
    require("stopped this recovery" in json.dumps(refused), "the new recovery file is void after the veto")
    node.client.connect()
    node.client.authenticate(network)
    require(node.client.principal["can_manage"] and node.client.own_id() in managers
            and node.client.call("directory")["view"]["epoch"] == view["epoch"],
            "existing managers keep management and the directory after the veto")
    return checkpoint


def verify_recovery_approval(node, other, network):
    material = other.command("recover_material", backup=node.backup, password=node.backup_password)
    other.connect(node.device_id)
    proof = other.command("recovery_read_proof", network_id=network)
    read = other.call("recovery_state", {**proof, "checkpoints": material["checkpoints"], "events": material["events"]})
    before = node.client.call("directory")
    keep = [*before["view"]["members"],
            {"device_id": other.own_id(), "kind": "client", "name": "Confirmed recovery", "manage": True}]
    checkpoint = other.command("recovery_checkpoint", state=read["state"], network_id=network,
                               members=keep, heads=read["heads"])
    other.call("recover", {"genesis": material["genesis"],
                           "checkpoints": [*read["state"]["checkpoints"], checkpoint],
                           "events": read["state"]["events"]})
    waiting = node.client.call("directory")
    pending = waiting["view"]["pending"]
    event = node.client.command("governance", state=waiting["state"], network_id=network,
                                action={"type": "approve", "checkpoint": pending["checkpoint"], "at_ms": waiting["now_ms"]})
    other.call("approve_recovery", {"event": event}, status=400)
    approved = node.client.call("approve_recovery", {"event": event})
    require(approved["view"]["epoch"] == checkpoint["epoch"] and approved["view"]["pending"] is None
            and approved["now_ms"] < pending["effective_at_ms"], "manager confirmation activates recovery before its deadline")
    require(node.client.call("approve_recovery", {"event": event})["view"] == approved["view"],
            "repeating the signed confirmation changes nothing")
    other.authenticate(network)
    require(other.principal["can_manage"] and other.principal["epoch"] == checkpoint["epoch"],
            "the waiting recovery client immediately becomes a manager")
    node.client.connect()
    node.client.authenticate(network)
    return checkpoint


def scheduler_records(node, network):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        connection.row_factory = sqlite3.Row
        rows = connection.execute("SELECT * FROM scheduler_jobs").fetchall()
    return [{**dict(row), "attempt": {"id": row["attempt_id"]} if row["attempt_id"] else None, "detail": row["detail"] or ""}
            for row in rows]


def scheduler_record(node, network, trigger):
    return next((row for row in scheduler_records(node, network) if row["trigger"] == trigger), None)


def require_quiet(description, predicate):
    # Cover two complete three-second durable scheduler reconciliation intervals.
    deadline = time.monotonic() + 7
    while time.monotonic() < deadline:
        require(predicate(), description)
        time.sleep(0.2)


def bot_checks(run, workspace, a, network, passed, outsider):
    definitions = [{"bot_id": "bot-" + label, "conversation_id": "bot-history-" + label,
                    "name": label.capitalize(), "instructions": "bot-" + label + "-instructions-sentinel",
                    "project": {"target_node": a.device_id, "cwd": str(workspace)}} for label in ("alpha", "beta")]
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
    require(sorted(definitions_of(a.client.rpc("bots")["bots"]), key=lambda bot: bot["bot_id"]) == stored,
            "Bot listing contains the two exact persisted definitions")
    require(outsider.client.rpc("bots")["bots"] == [], "another personal network cannot list these Bots")
    independent = outsider.client.rpc("create_bot", **{**definitions[0], "name": "Other network", "project": None})["bot"]
    require(definitions_of(outsider.client.rpc("bots")["bots"]) == [independent]
            and sorted(definitions_of(a.client.rpc("bots")["bots"]), key=lambda bot: bot["bot_id"]) == stored,
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
    a.stop()
    a.start()
    a.client.connect()
    a.client.authenticate()
    require_quiet("a restart never runs a model from Bot history", lambda: len(fixture_records(run)) == before_calls)
    require(sorted(definitions_of(a.client.rpc("bots")["bots"]), key=lambda bot: bot["bot_id"]) == stored,
            "Bot definitions and their signed identity survive restart")
    resumed = send(definitions[0], "bot-alpha-after-restart")
    require(resumed["resumed"] and resumed["native_session_id"] == first[0]["native_session_id"]
            and resumed["bot_prompt_markers"] == [definitions[0]["instructions"]],
            "restarted Bot resumes only its own native cache with its original instructions")
    passed("explicit Bot creation retries safely, isolates instructions/history/native caches and restarts without model replay",
           bot_ids=[bot["bot_id"] for bot in stored])


def node_cli(node, *arguments, socket=None, check=True, timeout=60, stdin=None):
    """The real `vibeface` CLI on the Node's machine, with the Node's private HOME."""
    environment = node.environment()
    environment["VIBEFACE_DATA_DIR"] = str(node.directory)
    if socket:
        environment["VIBEFACE_CODEX_SOCKET"] = str(socket)
    environment["VIBEFACE_TRIAGE_URL"] = f"http://127.0.0.1:{node.local_port}"
    completed = subprocess.run([str(node.binary), *arguments], capture_output=True, text=True, timeout=timeout,
                               env=environment, input=stdin)
    require(not check or completed.returncode == 0, "vibeface " + " ".join(arguments[:2]) + " succeeds: " + completed.stderr[-400:])
    return completed


def local_post(node, path, body):
    """A same-device caller on the Node's loopback-only listener, such as a hook."""
    request = urllib.request.Request(f"http://127.0.0.1:{node.local_port}{path}", json.dumps(body).encode(),
                                     {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)


def work_checks(run, workspace, a, network, passed):
    def rpc(method):
        return [row for row in fixture_records(run, "codex-rpc.jsonl") if row["message"].get("method") == method and not row["bot"]]

    def snapshot(name):
        return a.client.rpc("conversation", conversation_id=name)

    def work(name, work_id):
        return next((row for row in snapshot(name)["works"] if row["work_id"] == work_id), None)

    def in_state(name, work_id, *states):
        row = work(name, work_id)
        return row if row and row["state"] in states else None

    def app_server():
        return fixture_records(run, "codex-server.jsonl")[-1]

    def alive(pid):
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False

    def answers(thread_id):
        state = json.loads((run / "fixture-codex-state.json").read_text())
        request = state["threads"][thread_id].get("request") or {}
        return [row["message"] for row in fixture_records(run, "codex-rpc.jsonl")
                if "method" not in row["message"] and row["message"].get("id") == request.get("id")]

    def start(name, *, gate_open=False, report_results=True, native=None, **dispatch):
        directory = workspace / name
        directory.mkdir()
        if gate_open:
            (directory / "release").touch()
        a.client.rpc("create_conversation", conversation_id=name, title=name)
        receipt = a.client.rpc("message", conversation_id=name, message_id=name, report_results=report_results,
            text=json.dumps({"chat_agent": True, "label": name, "dispatch": {
                "label": name, "cwd": str(directory), **dispatch,
                "native": {"gate": str(directory / "release"), **(native or {})}}}))
        record = wait_for("the Agent dispatches one work", lambda: next(
            (row for row in fixture_records(run, "manager-native-work.jsonl") if row["label"] == name), None))
        wait_for("the dispatching turn ends after dispatch", lambda:
            (scheduler_record(a, network, receipt["trigger"]) or {}).get("state") == "completed")
        return directory, receipt, record["work_id"]

    def result_job(work_id):
        job = scheduler_record(a, network, "work:" + work_id)
        if job and job["state"] in {"failed", "interrupted", "cancelled"}:
            raise RuntimeError("work result job reached " + job["state"])
        return job if job and job["state"] == "completed" else None

    name = "chat-work-dedup"
    directory, receipt, work_id = start(name, gate_open=True, repeat=2, discard_first=True,
                                        changed="work ID reused with a different request")
    finished = wait_for("the native turn finishes and records its outcome", lambda: in_state(name, work_id, "finished"))
    require(finished["outcome"] == "completed" and finished["trigger"] == receipt["trigger"]
            and finished["target_node"] == a.device_id and finished["kind"] == "codex",
            "the work keeps its dispatching trigger, target and kind")
    job = wait_for("the finished work becomes its own result trigger", lambda: result_job(work_id))
    require(job["kind"] == "work_result", "the result comes back to the Agent as a new turn")
    results = [row for row in fixture_records(run, "manager-results.jsonl") if row["work_id"] == work_id]
    require(len(results) == 1 and results[0]["status_outcome"] == results[0]["outcome"] == "completed",
            "the result turn sees the finished work once")
    starts = [row for row in rpc("turn/start") if row["message"]["params"]["threadId"] == finished["thread_id"]]
    threads = [row for row in rpc("thread/start") if row["message"]["params"]["cwd"] == str(directory)]
    require(len(starts) == 1 and len(threads) == 1, "a repeated and discarded Shell dispatch starts exactly one turn")
    require(threads[0]["message"]["params"].get("approvalPolicy") == "never"
            and threads[0]["message"]["params"].get("sandbox") == "danger-full-access"
            and starts[0]["message"]["params"].get("approvalPolicy") == "never"
            and starts[0]["message"]["params"].get("sandboxPolicy") == {"type": "dangerFullAccess"}
            and starts[0]["message"]["params"].get("clientUserMessageId") == work_id,
            "the Agent's CLI starts the thread and turn itself with full access, keyed by the work ID")
    require(threads[0]["connection"] == starts[0]["connection"], "the Shell CLI's own App Server connection sends the start")
    replies = [message for message in snapshot(name)["messages"] if message["role"] == "assistant"]
    require(len(replies) == 2 and all(message["in_reply_to"] == name for message in replies),
            "the dispatch turn and the result turn each reply once to the original message")
    progress = [event for event in scope_events(a, network, name) if event["body"]["type"] == "work_progress"]
    require(progress and not any(marker in json.dumps(progress) for marker in FIXTURE_SECRET_MARKERS),
            "progress is persisted as events without command text, output or diffs")
    passed("dispatch_work from the Shell starts one full-access turn under retries; the result comes back once",
           work_id=work_id, result_trigger=job["trigger"], progress_events=len(progress))

    name = "chat-work-quiet"
    directory, receipt, work_id = start(name, gate_open=True, report_results=False)
    wait_for("the quiet work finishes", lambda: in_state(name, work_id, "finished"))
    require_quiet("report_results=false admits no result turn", lambda: scheduler_record(a, network, "work:" + work_id) is None)
    passed("a message that opts out of results finishes its work without a result turn", work_id=work_id)

    socket_path = app_server()["socket"]
    name = "chat-work-steer"
    directory, receipt, work_id = start(name)
    running = wait_for("the steered work is running", lambda: in_state(name, work_id, "running"))
    shell = node_cli(a, "codex", "send", running["thread_id"], "x", "--id", "shell-1", socket=socket_path, check=False)
    require(shell.returncode != 0 and "bot turn" in shell.stderr, "codex send outside a bot turn is refused")
    steer_label = name + "-send"
    steer_receipt = a.client.rpc("message", conversation_id=name, message_id=steer_label, report_results=True,
        text=json.dumps({"chat_agent": True, "label": steer_label, "send": {
            "thread": running["thread_id"], "text": "补充：只改测试", "id": "steer-1", "repeat": 2, "conflict_text": "换个说法"}}))
    action = wait_for("the bot sends into the running thread", lambda: next(
        (row for row in fixture_records(run, "manager-sends.jsonl") if row["label"] == steer_label), None))
    send_id = action["sent"][0]["work_id"]
    steers = [row for row in rpc("turn/steer") if row["message"]["params"]["threadId"] == running["thread_id"]]
    require(action["sent"][1]["work_id"] == send_id and action["sent"][1]["may_send"] is False
            and action["conflict"]["code"] != 0 and "reused with a different request" in action["conflict"]["error"]
            and len(steers) == 1 and steers[0]["message"]["params"]["expectedTurnId"] == running["turn_id"]
            and steers[0]["message"]["params"]["clientUserMessageId"] == send_id
            and not [row for row in rpc("turn/start") if row["message"]["params"].get("clientUserMessageId") == send_id],
            "codex send is a work that steers the running turn once; the same --id never sends again")
    steered = wait_for("the send work runs in the steered turn", lambda: in_state(name, send_id, "running"))
    require(steered["thread_id"] == running["thread_id"] and steered["turn_id"] == running["turn_id"]
            and steered["trigger"] == steer_receipt["trigger"], "the send work is bound to the turn it steered")
    wait_for("the sending turn ends after the send", lambda:
        (scheduler_record(a, network, steer_receipt["trigger"]) or {}).get("state") == "completed")
    listed = json.loads(node_cli(a, "codex", "call", "thread/list", "{}", socket=socket_path).stdout)
    models = json.loads(node_cli(a, "codex", "call", "model/list", socket=socket_path).stdout)
    renamed = node_cli(a, "codex", "call", "thread/name/set", json.dumps({"threadId": running["thread_id"], "name": "steered"}),
                       socket=socket_path)
    require(any(row["id"] == running["thread_id"] for row in listed["data"]) and models["data"][0]["id"] == "fixture-model"
            and renamed.returncode == 0, "codex call passes reads and thread management through")
    starts_before = len(rpc("turn/start")) + len(rpc("turn/steer"))
    refused = [node_cli(a, "codex", "call", method, json.dumps({"threadId": running["thread_id"]}), socket=socket_path, check=False)
               for method in ("turn/start", "turn/steer", "turn/interrupt")]
    require(all(result.returncode != 0 and "codex send" in result.stderr for result in refused)
            and len(rpc("turn/start")) + len(rpc("turn/steer")) == starts_before
            and in_state(name, work_id, "running"),
            "codex call refuses turn writes, so every turn stays a registered work")
    def resumes():
        return len([row for row in rpc("thread/resume") if row["message"]["params"].get("threadId") == running["thread_id"]])

    resumed = resumes()
    watcher = subprocess.Popen([str(a.binary), "codex", "watch", running["thread_id"]], stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, env={**a.environment(), "VIBEFACE_CODEX_SOCKET": socket_path})
    try:
        wait_for("codex watch subscribes to the thread", lambda: resumes() > resumed)
        (directory / "release").touch()
        watched = b""
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and b'"turn/completed"' not in watched:
            if select.select([watcher.stdout], [], [], 1)[0]:
                chunk = os.read(watcher.stdout.fileno(), 65536)
                if not chunk:
                    break
                watched += chunk
        require(b'"turn/completed"' in watched, "codex watch streams the thread's notifications")
    finally:
        watcher.kill()
        watcher.wait(timeout=5)
    shared = [wait_for("both works finish with the shared turn", lambda: in_state(name, work, "finished"))
              for work in (work_id, send_id)]
    require(all(row["result_text"] == "fixture Codex terminal result" for row in shared),
            "every work in a shared turn receives the turn's answer")
    wait_for("the steered work reports its result", lambda: result_job(work_id))
    wait_for("the send work reports its own result", lambda: result_job(send_id))
    passed("a send steers the running turn as its own work; the Shell lists, calls and watches the App Server directly",
           work_id=work_id, send_work=send_id, steer_turn=running["turn_id"])

    name = "chat-work-unknown"
    directory, receipt, work_id = start(name, gate_open=True, native={"drop_reply": True})
    finished = wait_for("the Node settles the unknown send from the thread", lambda: in_state(name, work_id, "finished"), timeout=90)
    starts = [row for row in rpc("turn/start") if row["message"]["params"].get("clientUserMessageId") == work_id]
    require(finished["outcome"] == "completed" and len(starts) == 1,
            "a lost turn/start reply is reconciled by clientUserMessageId and never resent")
    passed("an unknown turn/start result is reconciled by its client ID without a second send", work_id=work_id)

    name = "chat-work-request"
    directory, receipt, work_id = start(name, gate_open=True,
                                        native={"request": "item/commandExecution/requestApproval", "request_mode": "respond"})
    finished = wait_for("the Agent answers the held request and the work finishes", lambda: in_state(name, work_id, "finished"), timeout=90)
    replies = answers(finished["thread_id"])
    actions = wait_for("the triage turn records its answers", lambda: [row for row in fixture_records(run, "triage-actions.jsonl")
                       if row["session_id"] == finished["thread_id"] and row.get("request_id")])
    require(len(replies) == 1 and replies[0]["result"] == {"decision": "accept"} and len(actions) == 1
            and actions[0]["answers"][0] == actions[0]["answers"][1] and actions[0]["answers"][0]["by"] == "agent",
            "the coordination Agent answers the App Server request once through respond; a repeat returns the first answer")
    passed("a server request goes to the coordination Agent, which answers it once", work_id=work_id)

    name = "chat-work-ignored"
    directory, receipt, work_id = start(name, gate_open=True,
                                        native={"request": "item/fileChange/requestApproval", "request_mode": "ignore"})
    finished = wait_for("an unanswered request gets the fallback and the work finishes", lambda: in_state(name, work_id, "finished"), timeout=90)
    replies = answers(finished["thread_id"])
    require(len(replies) == 1 and replies[0]["result"] == {"decision": "decline"},
            "a request the Agent neither answers nor cards is declined once when its turn ends")
    passed("a request left alone is declined deterministically and recorded", work_id=work_id)

    name = "chat-work-question"
    directory, receipt, work_id = start(name, gate_open=True,
                                        native={"request": "item/commandExecution/requestApproval", "request_mode": "ask"})
    question = wait_for("the Agent asks in the work's chat about the held request", lambda: next(
        (row for row in a.client.rpc("questions")["questions"] if row["conversation_id"] == name), None), timeout=90)
    thread_id, question_id = question["about"]["session_id"], question["question_id"]
    asked = wait_for("the triage turn records its question", lambda: [row for row in fixture_records(run, "triage-actions.jsonl")
                     if row["session_id"] == thread_id])
    require([choice["label"] for choice in question["choices"]] == ["允许", "拒绝"] and question["about"]["request_id"]
            and thread_id == (work(name, work_id) or {}).get("thread_id") and len(asked) == 1
            and asked[0]["asked"][0]["question_id"] == asked[0]["asked"][1]["question_id"] == question_id,
            "the question about the request lands once in the chat that owns the thread")
    require(any(row["question_id"] == question_id and row["answer"] is None for row in snapshot(name)["questions"]),
            "the conversation shows the unanswered question")
    require_quiet("a request a question holds waits for the answer", lambda: in_state(name, work_id, "running") and not answers(thread_id))
    first = a.client.rpc("answer", question_id=question_id, text="拒绝", scope_id=name)
    retry = a.client.rpc("answer", question_id=question_id, text="拒绝", scope_id=name)
    other = a.client.rpc("answer", question_id=question_id, text="允许", scope_id=name)
    require(first["status"] == retry["status"] == "answered" and retry["message"] == first["message"]
            and first["message"]["message_id"] == "a-" + question_id and first["message"]["answers"] == question_id
            and other["status"] == "already_answered" and other["message"]["text"] == "拒绝",
            "the answer is written once; a retry reads it back and a different answer finds the first")
    finished = wait_for("the bot acts on the answer and the work finishes", lambda: in_state(name, work_id, "finished"), timeout=90)
    replies = answers(finished["thread_id"])
    turns = [row for row in fixture_records(run) if row["kind"] == "answer" and row["conversation_id"] == name]
    acted = wait_for("the answer turn records its replies", lambda: [row for row in fixture_records(run, "answer-actions.jsonl")
                     if row["question_id"] == question_id])
    require(len(replies) == 1 and replies[0]["result"] == {"decision": "decline"} and len(turns) == 1
            and turns[0]["trigger"] == "message:a-" + question_id and len(acted) == 1
            and acted[0]["replies"][0] == acted[0]["replies"][1] and acted[0]["replies"][0]["by"] == "agent",
            "the answer is one turn of the bot, whose respond reaches Codex once")
    require(all(row["question_id"] != question_id for row in a.client.rpc("questions")["questions"])
            and any(row["question_id"] == question_id and row["answer"]["text"] == "拒绝" for row in snapshot(name)["questions"]),
            "the answered question leaves the pending list and shows its answer in the chat")
    passed("a question in the chat holds the request until the user's answer reaches it once through the bot",
           work_id=work_id, question=question_id)

    name = "chat-work-interrupt"
    directory, receipt, work_id = start(name)
    running = wait_for("the work is running", lambda: in_state(name, work_id, "running"))
    turns_before = len(rpc("turn/start"))
    response = a.client.rpc("interrupt_work", conversation_id=name, work_id=work_id)
    require(response["interrupt_requested"] and not response["confirmed"], "interrupt admission is not confirmation")
    stopped = wait_for("the native turn confirms the interrupt", lambda: in_state(name, work_id, "finished"))
    interrupts = [row for row in rpc("turn/interrupt") if row["message"]["params"]["threadId"] == running["thread_id"]]
    require(stopped["outcome"] == "interrupted" and len(interrupts) == 1
            and interrupts[0]["message"]["params"] == {"threadId": running["thread_id"], "turnId": running["turn_id"]},
            "the interrupt selects exactly the original thread and turn")
    require(len(rpc("turn/start")) == turns_before, "an interrupt never restarts work")
    passed("interrupt_work stops exactly the original turn and confirms it from the App Server", work_id=work_id)

    name = "chat-work-crash"
    directory, receipt, work_id = start(name)
    running = wait_for("the crash work is running", lambda: in_state(name, work_id, "running"))
    old_server = app_server()["pid"]
    require(alive(old_server), "the App Server runs before the crash")
    turns_before = len(rpc("turn/start"))
    a.stop(crash=True)
    require(alive(old_server), "a killed Node leaves its App Server group behind")
    state_path = run / "fixture-codex-state.json"
    state = json.loads(state_path.read_text())
    thread = state["threads"][running["thread_id"]]
    thread["status"] = {"type": "idle"}
    thread["turns"][-1].update(status="completed")
    thread["turns"][-1]["items"].append({"id": "offline-answer", "type": "agentMessage",
                                          "text": "fixture completed while Node was offline"})
    private_json(state_path, state)
    a.start()
    a.client.connect()
    a.client.authenticate()
    wait_for("the next start stops the orphaned App Server", lambda: not alive(old_server))
    finished = wait_for("a probe reads the completion missed while offline", lambda: in_state(name, work_id, "finished"))
    require(finished["outcome"] == "completed" and "offline" in finished["result_text"]
            and len(rpc("turn/start")) == turns_before, "recovery reads the real result and never starts another turn")
    wait_for("the recovered work reports its result", lambda: result_job(work_id))
    passed("a Node crash kills the orphaned App Server on restart and reconciles the missed result without replay",
           work_id=work_id)

    name = "chat-work-lost"
    directory, receipt, work_id = start(name)
    running = wait_for("the lost work is running", lambda: in_state(name, work_id, "running"))
    a.stop(crash=True)
    state = json.loads(state_path.read_text())
    del state["threads"][running["thread_id"]]
    private_json(state_path, state)
    a.start()
    a.client.connect()
    a.client.authenticate()
    lost = wait_for("an unreadable turn becomes unverified", lambda: in_state(name, work_id, "unverified"))
    check_id = "work:" + work_id
    require_quiet("an unverified work never reports or reruns", lambda:
        scheduler_record(a, network, "work:" + work_id) is None
        and len([row for row in rpc("thread/start") if row["message"]["params"]["cwd"] == str(directory)]) == 1)
    a.client.rpc("resolve", conversation_id=name, check_id=check_id, outcome="interrupted",
                 note="Fixture thread was deleted; no file in the work directory changed.")
    resolved = wait_for("the user's choice settles the work", lambda: in_state(name, work_id, "finished"))
    passed("an unreadable turn stays unverified until the user settles it",
           work_id=work_id, outcome=resolved["outcome"], evidence=lost.get("failure_reason"))

    server = app_server()["pid"]
    require(alive(server), "the restarted App Server is running")
    a.stop()
    require(not alive(server), "SIGTERM stops the managed App Server group")
    (run / "fixture-codex-slow-start").write_text("8")
    a.start()
    a.client.connect()
    a.client.authenticate()
    passed("a clean stop leaves no App Server behind")

    name = "chat-work-cold-start"
    directory, receipt, work_id = start(name, gate_open=True)
    finished = wait_for("the cold-start work finishes", lambda: in_state(name, work_id, "finished", "failed", "dispatch_rejected"), timeout=90)
    starts = [row for row in rpc("turn/start") if row["message"]["params"].get("clientUserMessageId") == work_id]
    require(finished["state"] == "finished" and finished["outcome"] == "completed" and len(starts) == 1,
            f"a dispatch before the App Server is up waits for it and sends once: {finished}")
    wait_for("the cold-start work reports its result", lambda: result_job(work_id))
    passed("a dispatch right after the Node starts waits for its App Server and starts one turn", work_id=work_id)


class WebhookReceiver:
    """A local Grok Bot stand-in: checks each signature and answers per event from a script of statuses."""

    def __init__(self, secret, scripted):
        import http.server
        receiver = self
        self.secret, self.scripted, self.deliveries = secret.encode(), scripted, []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                timestamp, event_id = self.headers["X-Vibeface-Timestamp"], self.headers["X-Vibeface-Event-Id"]
                expected = "v1=" + hmac.new(receiver.secret, (timestamp + ".").encode() + body, hashlib.sha256).hexdigest()
                valid = hmac.compare_digest(expected, self.headers["X-Vibeface-Signature"]) and abs(time.time() - int(timestamp)) < 300
                payload = json.loads(body)
                statuses = [200] if payload.get("test") else receiver.scripted.get((payload.get("about") or {}).get("session_id"), [500, 200])
                seen = sum(row["event_id"] == event_id for row in receiver.deliveries)
                status = statuses[min(seen, len(statuses) - 1)] if valid else 401
                receiver.deliveries.append({"event_id": event_id, "valid": valid, "status": status, "body": payload})
                self.send_response(status)
                self.end_headers()

            def log_message(self, *arguments):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/grok"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()


def triage_checks(run, workspace, a, network, passed):
    """Hooks feed the Agent's triage; questions reach the owning chat and the signed webhook; an answer returns once."""
    secret = uuid.uuid4().hex + uuid.uuid4().hex
    receiver = WebhookReceiver(secret, {"dead-session": [400]})
    try:
        raise_dir = workspace / "triage-raise"
        raise_dir.mkdir()
        shared_raise_dir = "workspace-1/triage-raise"
        settings = a.client.call("configure_triage", {"default_outlet": "both", "webhook_url": receiver.url,
                                 "scan_codex_sessions": True, "codex_sessions": str(a.directory / "home/.codex/sessions"), "webhook_secret": secret})
        require(settings["webhook_secret_set"] and secret not in json.dumps(settings)
                and secret not in (a.directory / "config.json").read_text(), "the webhook secret is write-only and stays out of config")
        tested = a.client.call("test_webhook")
        require(tested["ok"] and receiver.deliveries[-1]["valid"], "the test button sends one signed request")
        threads = json.loads((run / "fixture-codex-state.json").read_text())["threads"]
        owner = "chat-work-dedup"
        thread_id = next(thread_id for thread_id, thread in threads.items() if thread["cwd"].endswith(owner))
        completed_turn = threads[thread_id]["turns"][-1]["id"]
        replayed = node_cli(a, "hook", "codex", stdin=json.dumps({"hook_event_name": "Stop", "session_id": thread_id,
                            "turn_id": completed_turn, "cwd": threads[thread_id]["cwd"], "last_assistant_message": "done"}))
        hook = {"hook_event_name": "Stop", "session_id": thread_id, "turn_id": "hook-turn-1", "cwd": str(raise_dir),
                "last_assistant_message": "需要你决定下一步 needs-human"}
        for _ in range(2):
            node_cli(a, "hook", "codex", stdin=json.dumps(hook))
        tested_hook = json.loads(node_cli(a, "hook", "codex", "--test").stdout)
        require(tested_hook["status"] == "accepted" and replayed.returncode == 0, "the hook CLI reaches the local Node")
        question = wait_for("the Agent asks in the chat that owns the session", lambda: next(
            (row for row in a.client.rpc("questions")["questions"] if row["about"]["cwd"] == shared_raise_dir), None), timeout=90)
        question_id = question["question_id"]
        require(question["conversation_id"] == owner and question["severity"] == "attention"
                and question["about"]["session_id"] == thread_id and question["about"]["node"] == a.device_id,
                "the question names the chat, session and device it is about")
        wait_for("the triage turn records its question", lambda: any(row["cwd"] == shared_raise_dir and row["session_id"] == thread_id
                 for row in fixture_records(run, "triage-actions.jsonl")))
        actions = fixture_records(run, "triage-actions.jsonl")
        raised = [row for row in actions if row["cwd"] == shared_raise_dir and row["session_id"] == thread_id]
        same_turn = [row for row in actions if row["session_id"] == thread_id and row["cwd"] != shared_raise_dir
                     and not row.get("request_id")]
        require(len(raised) == 1 and len(same_turn) == 1,
                "a repeated hook and the App Server's report of the same turn reach the Agent once")
        signed = wait_for("the webhook gets the question after one retry", lambda: (lambda rows: rows if len(rows) >= 2 else None)(
            [row for row in receiver.deliveries if row["event_id"] == question_id]), timeout=60)
        require(all(row["valid"] for row in signed) and [row["status"] for row in signed] == [500, 200]
                and signed[-1]["body"]["type"] == "question" and signed[-1]["body"]["answer"]["question_id"] == question_id,
                "a 5xx is retried with the same event_id until accepted")
        require(str(workspace) not in json.dumps(signed), "the signed webhook must not expose the local workspace path")
        dead_hook = {"hook_event_name": "Stop", "session_id": "dead-session", "turn_id": "dead-turn", "cwd": str(raise_dir),
                     "last_assistant_message": "dead-letter needs-human"}
        node_cli(a, "hook", "codex", stdin=json.dumps(dead_hook))
        wait_for("a 4xx goes to the dead letters", lambda: a.client.call("triage_settings")["outbox"]["dead"] >= 1, timeout=90)
        foreign = next(row for row in a.client.rpc("questions")["questions"] if row["about"]["session_id"] == "dead-session")
        primary = a.client.rpc("bots")["primary"]
        require(foreign["conversation_id"] == primary and [row["action"] for row in fixture_records(run, "triage-actions.jsonl")
                if row["session_id"] == "dead-session"] == ["raise"], "a session no bot started goes to the primary bot and is triaged there")

        first = a.client.rpc("answer", question_id=question_id, text="继续，先跑测试", scope_id=owner)
        retry = a.client.rpc("answer", question_id=question_id, text="继续，先跑测试", scope_id=owner)
        acted = wait_for("the bot passes the answer on", lambda: [row for row in fixture_records(run, "answer-actions.jsonl")
                         if row["question_id"] == question_id] or None, timeout=90)
        answer_work = acted[0]["replies"][0]["work_id"]
        sends = [row for row in fixture_records(run, "codex-rpc.jsonl") if row["message"].get("method") in ("turn/start", "turn/steer")
                 and row["message"]["params"].get("clientUserMessageId") == answer_work]
        require(first["status"] == "answered" and retry["message"] == first["message"] and len(acted) == 1
                and acted[0]["replies"][1]["work_id"] == answer_work and len(sends) == 1,
                "a text answer reaches the session once as the bot's work; a retry reads it back")
        told = wait_for("the webhook hears the answer", lambda: [row for row in receiver.deliveries
                        if row["event_id"].startswith("answered-" + question_id + "-") and row["status"] == 200] or None, timeout=60)
        require(told[-1]["body"] == {"schema_version": 1, "type": "answered", "event_id": told[-1]["event_id"],
                                     "question_id": question_id, "text": "继续，先跑测试"}, "the webhook learns the answer given in the web")

        # The decision token answers a question about a live session, so the bot's answer turn sends real work.
        decide_dir = workspace / "triage-raise-decision"
        decide_dir.mkdir()
        node_cli(a, "hook", "codex", stdin=json.dumps({"hook_event_name": "Stop", "session_id": thread_id, "turn_id": "hook-turn-decision",
                                                       "cwd": str(decide_dir), "last_assistant_message": "choose-next needs-human"}))
        pending = wait_for("the Agent asks about the live session again", lambda: next(
            (row for row in a.client.rpc("questions")["questions"] if row["about"]["cwd"] == "workspace-1/triage-raise-decision"), None), timeout=90)
        tokens = a.client.call("create_decision_token", {"label": "grok"})
        listed = a.client.call("triage_settings")
        require(tokens["token"] not in json.dumps(listed) and tokens["token"] not in (a.directory / "decision-tokens.json").read_text(),
                "a decision token is shown once and stored hashed")
        decision = {"device_id": a.device_id, "question_id": pending["question_id"], "choice": 0}
        bearer = {"Authorization": "Bearer " + tokens["token"]}
        posted = a.client.request("/api/decision", decision, origin=False, headers=bearer)
        again = a.client.request("/api/decision", decision, origin=False, headers=bearer)
        a.client.request("/api/decision", decision, origin=False, headers={"Authorization": "Bearer wrong"}, status=401)
        require(posted == again == {"question_id": pending["question_id"], "answer_id": "a-" + pending["question_id"],
                                    "status": "answered", "text": "继续"}, "a decision token answers one question over HTTP once")
        decided = wait_for("the bot passes the decision on", lambda: [row for row in fixture_records(run, "answer-actions.jsonl")
                           if row["question_id"] == pending["question_id"]] or None, timeout=90)
        decided_work = decided[0]["replies"][0]["work_id"]
        decided_sends = [row for row in fixture_records(run, "codex-rpc.jsonl") if row["message"].get("method") in ("turn/start", "turn/steer")
                         and row["message"]["params"].get("clientUserMessageId") == decided_work]
        handle = decided[0]["replies"][0]["state"]
        require(len(decided) == 1 and decided[0]["answer"] == "继续" and decided[0]["replies"][1]["work_id"] == decided_work
                and len(decided_sends) == 1 and handle["state"] == "running" and handle["handle"]["thread_id"] == thread_id
                and not any((row["message"].get("params") or {}).get("threadId") == "dead-session"
                            for row in fixture_records(run, "codex-rpc.jsonl")),
                f"a decision token's answer reaches the live session once as the bot's work, and nothing targets a dead one: {handle}")
        a.client.call("revoke_decision_token", {"token_id": tokens["token_id"]})
        a.client.request("/api/decision", decision, origin=False, headers=bearer, status=401)
        log = (a.directory / "process.log").read_text(errors="replace")
        require(secret not in log and tokens["token"] not in log, "logs carry neither the webhook secret nor the token")
        passed("hooks reach the Agent once; questions go to the owning chat and a signed, retried webhook; answers return once",
               question=question_id, webhook_deliveries=len(receiver.deliveries))
    finally:
        receiver.close()


def foreign_session_checks(run, workspace, a, network, passed):
    """A session no bot started goes to the primary bot once, with no question; that bot sees it in its turn and hands
    it to another bot or stops following it with hand_over_session, and a session nobody drives is left alone."""
    labels = ("keep", "hand", "mute", "next", "exec")
    sessions = {label: "foreign-" + label + "-" + uuid.uuid4().hex[:8] for label in labels}
    directory = workspace / "foreign"
    directory.mkdir()
    rollout = directory / "exec-rollout.jsonl"
    rollout.write_text(json.dumps({"type": "session_meta", "payload": {"id": sessions["exec"], "source": "exec",
                                   "originator": "codex_exec"}}) + "\n")

    def hook(session, turn, message="tests fail", **extra):
        node_cli(a, "hook", "codex", stdin=json.dumps({"hook_event_name": "Stop", "session_id": session, "turn_id": turn,
                                                       "cwd": str(directory), "last_assistant_message": message, **extra}))

    def listed(conversation):
        return {row["session_id"]: row for row in a.client.rpc("conversation", conversation_id=conversation)["sessions"]}

    def triaged(session):
        return [row for row in fixture_records(run, "triage-actions.jsonl") if row["session_id"] == session]

    def owner_events(kind):
        return [event["body"] for scope in ("bot-history-alpha", "bot-history-beta") for event in scope_events(a, network, scope)
                if event["body"]["type"] == kind and event["body"]["session_id"] in sessions.values()]

    versions = {}

    def hand_over(label, **how):
        return {"name": "hand_over_session", "arguments": {"node": a.device_id, "session_id": sessions[label],
                                                           "version": versions[label], **how}}

    require(a.client.rpc("bots")["primary"] == "bot-history-alpha", "the first bot created is the primary one")
    for label in ("keep", "hand", "mute"):
        hook(sessions[label], "t1")
        hook(sessions[label], "t2")
    hook(sessions["exec"], "t1", "needs-human", transcript_path=str(rollout))
    wait_for("each session goes to the primary bot", lambda: all(sessions[label] in listed("bot-history-alpha")
             for label in ("keep", "hand", "mute")), timeout=60)
    wait_for("the attributed sessions reach the Agent", lambda: all(len(triaged(sessions[label])) == 2 for label in ("keep", "hand", "mute")), timeout=90)
    require(not any(row["about"]["session_id"] in sessions.values() for row in a.client.rpc("questions")["questions"]),
            "nobody is asked who takes a session")
    require(sorted(body["session_id"] for body in owner_events("session_attributed")) == sorted(sessions[label] for label in ("keep", "hand", "mute"))
            and all(not row["muted"] for row in listed("bot-history-alpha").values()), "a repeated hook attributes a session once")

    alpha = listed("bot-history-alpha")
    versions.update({label: alpha[sessions[label]]["version"] for label in ("hand", "mute")})
    receipt = a.client.rpc("message", conversation_id="bot-history-alpha", message_id="hand-over", text=json.dumps({
        "chat_agent": True, "tools": [
            hand_over("hand", to="bot-history-beta"), hand_over("hand", to="bot-history-beta"),
            {**hand_over("hand", stop_following=True), "accepted": False, "rejection": "follows it"},
            hand_over("mute", stop_following=True), hand_over("mute", stop_following=True),
            {**hand_over("mute", to="bot-history-beta"), "accepted": False, "rejection": "it is at version"}]}))
    wait_for("the bot's hand-over turn finishes", lambda:
             (scheduler_record(a, network, receipt["trigger"]) or {}).get("state") == "completed", timeout=90)
    seen = next(row for row in fixture_records(run) if row["trigger"] == receipt["trigger"])
    require({sessions[label] for label in ("keep", "hand", "mute")} <= set(seen["sessions"]),
            "the bot's turn lists the sessions it follows")
    calls = [row["result"] for row in fixture_records(run, "manager-tools.jsonl")
             if row["trigger"] == receipt["trigger"] and row["name"] == "hand_over_session" and row["accepted"]]
    handed, muted = calls[:2], calls[2:]
    require([row["changed"] for row in handed] == [True, False] and handed[0]["conversation_id"] == "bot-history-beta",
            "hand_over_session hands the session over once; the retry reads back what stands")
    require([row["changed"] for row in muted] == [True, False] and muted[0]["muted"]
            and muted[0]["version"] == muted[1]["version"] != versions["mute"], "stop_following is written once")
    wait_for("the hand-over moves the session between chats", lambda: sessions["hand"] in listed("bot-history-beta")
             and sessions["hand"] not in listed("bot-history-alpha"), timeout=60)
    require(listed("bot-history-alpha")[sessions["mute"]]["muted"], "the muted session stays in its bot's chat, marked")

    a.client.rpc("set_primary_bot", conversation_id="bot-history-beta")
    hook(sessions["next"], "t1")
    wait_for("the next session goes to the new primary", lambda: sessions["next"] in listed("bot-history-beta"), timeout=60)

    before = {label: len(triaged(sessions[label])) for label in labels}
    for label in ("mute", "hand"):
        hook(sessions[label], "t3", "needs-human")
    later = wait_for("a later question about the handed-over session lands in its new bot's chat", lambda: next(
        (row for row in a.client.rpc("questions")["questions"] if row["about"]["session_id"] == sessions["hand"]
         and row["conversation_id"] == "bot-history-beta"), None), timeout=90)
    require_quiet("a muted session and one nobody drives stay quiet and unowned", lambda: len(triaged(sessions["mute"])) == before["mute"]
                  and not triaged(sessions["exec"]) and sessions["exec"] not in listed("bot-history-alpha")
                  and sessions["exec"] not in listed("bot-history-beta"))
    attributed = owner_events("session_attributed")
    require(sorted(body["session_id"] for body in attributed) == sorted(sessions[label] for label in ("keep", "hand", "hand", "mute", "next"))
            and [body["session_id"] for body in owner_events("session_muted")] == [sessions["mute"]],
            "each attribution, hand-over and mute is written exactly once")
    passed("a session no bot started goes to the primary bot once; its bot hands it over and stops following it once each; "
           "a codex exec session is skipped",
           kept=sessions["keep"], handed=sessions["hand"], muted=sessions["mute"], next=sessions["next"],
           skipped=sessions["exec"], later=later["question_id"])


def no_agent_checks(run, a, passed):
    """Without a coordination Agent a server request is answered by the fallback at once and events are dropped."""
    a.stop()
    a.configure(manager=None)
    a.start()
    a.client.connect()
    a.client.authenticate()
    dropped = local_post(a, "/api/hooks", {"source": "hook", "agent": "codex", "event": "turn_end",
                                           "session_id": "no-agent", "turn_id": "t", "cwd": "/tmp"})
    require(dropped["status"] == "dropped", "without an Agent an event is dropped, not queued")
    marker = run / "fixture-codex-global-request"
    marker.write_text(uuid.uuid4().hex[:8])
    request_id = "global-" + marker.read_text()
    answer = wait_for("the fallback answers the request at once", lambda: next(
        (row["message"] for row in fixture_records(run, "codex-rpc.jsonl") if row["message"].get("id") == request_id), None))
    require("error" in answer or "result" in answer, "a credential refresh with no Agent gets its fallback answer")
    passed("with no coordination Agent, requests get the deterministic fallback and events are dropped", request=request_id,
           answer=answer.get("result", answer.get("error")))


def triage_budget_checks(run, workspace, a, network, passed):
    settings = a.client.call("triage_settings")
    config = {key: settings[key] for key in ("default_outlet", "projects", "webhook_url", "scan_codex_sessions", "codex_sessions")}
    a.client.call("configure_triage", {**config, "scan_codex_sessions": False})
    wait_for("previous triage batches settle", lambda: (value := a.client.call("triage_settings")["status"])["inflight_events"] == 0
             and value["pending_events"] == 0, timeout=90)
    day, limit = settings["status"]["budget"]["day"], settings["status"]["budget"]["limit"]

    def usage(used):
        with sqlite3.connect(a.directory / "node.sqlite3") as connection:
            connection.execute("INSERT INTO budget_days(network_id,day,used) VALUES(?,?,?) "
                               "ON CONFLICT(network_id,day) DO UPDATE SET used=excluded.used", ("triage:" + network, day, used))

    def status():
        return a.client.call("triage_settings")["status"]

    def event(session, hold=False):
        return {"source": "hook", "agent": "claude" if hold else "codex", "event": "needs_input" if hold else "turn_end",
                "session_id": session, "turn_id": "budget-turn", "cwd": str(workspace),
                **({"hold": "permission"} if hold else {})}

    def invocations():
        return fixture_records(run, "triage-invocations.jsonl")

    def actions(session):
        return [row for row in fixture_records(run, "triage-actions.jsonl") if row["session_id"] == session]

    usage(limit)
    before = len(invocations())
    ordinary = event("budget-ordinary")
    local_post(a, "/api/hooks", ordinary)
    held = local_post(a, "/api/hooks", event("budget-deadline", hold=True))
    paused = wait_for("budget denial leaves events pending", lambda: (value if
        (value := status())["budget"]["paused_reason"] == "budget_exhausted" and value["pending_events"] == 2 else None))
    require(paused["budget"]["used"] == paused["budget"]["limit"] == limit and paused["budget"]["retry_at_ms"] > time.time() * 1000,
            "the existing settings response gives usage and the UTC retry boundary")
    requests = json.loads(node_cli(a, "requests").stdout)["requests"]
    require(any(row["request_id"] == held["request_id"] for row in requests), "budget denial preserves the original held request")
    wait_for("the original hook deadline falls back", lambda: not json.loads(node_cli(a, "requests").stdout)["requests"], timeout=45)
    expired = json.loads(node_cli(a, "triage", "events").stdout)["events"]
    require(len(expired) == 2 and all(row.get("request_id") is None for row in expired) and len(invocations()) == before,
            "deadline fallback retains both unreviewed events without a model call")
    local_post(a, "/api/hooks", event("budget-orphan", hold=True))
    a.stop(crash=True)
    stop_fixture_processes(run)
    a.start()
    a.client.connect()
    a.client.authenticate()
    require(status()["pending_events"] == 3 and not json.loads(node_cli(a, "requests").stdout)["requests"],
            "restart retains Pending events without recreating the orphan transport")
    usage(0)
    wait_for("restored budget reviews every retained event", lambda: all(len(actions(session)) == 1 for session in
             ("budget-ordinary", "budget-deadline", "budget-orphan")) and status()["inflight_events"] == 0, timeout=90)
    require(len(invocations()) == before + 1 and status()["pending_events"] == 0,
            "one recovered batch invokes exactly once")
    require(local_post(a, "/api/hooks", ordinary)["status"] == "duplicate", "repeat scans keep the same seen event")
    require_quiet("repeat scans do not replay actions", lambda: len(invocations()) == before + 1)
    passed("triage budget denial retains events, deadline fallback keeps review data and recovery runs one batch",
           used=paused["budget"]["used"], limit=limit, recovered_actions=3)

    hold = run / "hold-triage"
    hold.touch()
    unknown = local_post(a, "/api/hooks", event("budget-unknown"))
    accepted = wait_for("the controlled coordination turn starts before its crash", lambda:
                        invocations()[-1] if len(invocations()) == before + 2 and status()["inflight_events"] == 1 else None, timeout=90)
    require(accepted["attempt"], "the real CLI turn receives an exact attempt context")
    a.stop(crash=True)
    stop_fixture_processes(run)
    hold.unlink()
    a.start()
    a.client.connect()
    a.client.authenticate()
    fenced = status()
    require(fenced["unknown_events"] == 1 and fenced["budget"]["used"] == 2,
            "an accepted turn keeps its charge and becomes observable Unknown after restart")
    late_env = {**a.environment(), "VIBEFACE_DATA_DIR": str(a.directory), "VIBEFACE_TRIAGE_URL": f"http://127.0.0.1:{a.local_port}",
                "VIBEFACE_TRIAGE_ATTEMPT": accepted["attempt"]}
    late = subprocess.run([str(a.binary), "triage", "dismiss", "--event-id", unknown["event_id"]],
                          env=late_env, capture_output=True, text=True, timeout=30)
    require(late.returncode != 0 and "no longer active" in late.stderr, "an old CLI attempt cannot mutate Unknown evidence")
    local_post(a, "/api/hooks", event("budget-positive-after-unknown"))
    wait_for("a fresh Pending event remains reviewable beside Unknown", lambda: len(actions("budget-positive-after-unknown")) == 1
             and status()["inflight_events"] == 0, timeout=90)
    require(not actions("budget-unknown") and status()["unknown_events"] == 1 and len(invocations()) == before + 3,
            "Unknown is neither refunded nor replayed while fresh work proceeds")
    passed("triage crash fencing preserves charges, rejects stale CLI writes and allows fresh Pending work",
           unknown_events=1, late_attempt_refused=True)


def bot_settings_checks(run, a, network, passed):
    """The user's own message changes the bot through tools; a schedule runs once per time."""
    name = "bot-history-alpha"

    def send(message_id, text, conversation=name):
        receipt = a.client.rpc("message", conversation_id=conversation, message_id=message_id, text=text)
        wait_for("the settings turn gets its final reply", lambda:
            (scheduler_record(a, network, receipt["trigger"]) or {}).get("state") == "completed")
        return receipt

    def snapshot(conversation=name):
        return a.client.rpc("conversation", conversation_id=conversation)

    def results(trigger, tool_name):
        return [row for row in fixture_records(run, "manager-tools.jsonl")
                if row["trigger"] == trigger and row["name"] == tool_name]

    def replies_to(prefix, conversation=name):
        return [message for message in snapshot(conversation)["messages"]
                if (message.get("in_reply_to") or "").startswith(prefix)]

    first_run = int(time.time() * 1000) + 3000
    profile = {"call_ref": "profile", "name": "Nova", "instructions": "bot-nova-instructions"}
    schedule = {"call_ref": "schedule", "name": "Ping", "prompt": "Reply with one line saying the schedule ran; "
                "if nothing is available, say so.", "first_run_at_ms": first_run, "every_minutes": 5}
    receipt = send("bot-settings", json.dumps({"chat_agent": True, "tools": [
        {"name": "update_profile", "arguments": profile},
        {"name": "update_profile", "arguments": profile},
        {"name": "update_profile", "arguments": {**profile, "name": "Other"}, "accepted": False,
         "rejection": "different change"},
        {"name": "update_profile", "arguments": {"call_ref": "model", "model": "other"}, "accepted": False,
         "rejection": "only in Settings"},
        {"name": "save_skill", "arguments": {"call_ref": "skill", "name": "Digest", "description": "Summarize commits",
                                             "body": "List yesterday's commits by author."}},
        {"name": "save_schedule", "arguments": schedule},
        {"name": "call_lookup", "arguments": {"call_ref": "profile"}},
    ]}))
    first, retry = results(receipt["trigger"], "update_profile")[:2]
    lookup = results(receipt["trigger"], "call_lookup")[0]["result"]
    require(first["result"] == retry["result"] == lookup and first["result"]["cause"]["message_id"] == "bot-settings",
            "a retried or looked-up call_ref returns the one recorded change")
    view = snapshot()
    changes = view["changes"]
    require([change["change"]["kind"] for change in changes] == ["profile", "skill", "schedule"]
            and len({change["change_id"] for change in changes}) == 3
            and all(change["cause"]["message_id"] == "bot-settings" and change["event_hash"] for change in changes),
            "the conversation lists one change record per applied change, without the retry")
    require(changes[0]["change"]["name"] == {"before": "Alpha", "after": "Nova"}
            and view["bot"]["name"] == "Nova" and view["bot"]["instructions"] == "bot-nova-instructions"
            and any(bot["name"] == "Nova" for bot in a.client.rpc("bots")["bots"]),
            "the merged profile shows in the conversation and the bots listing")
    skill = next(row for row in view["bot_settings"]["skills"] if row["name"] == "Digest")
    saved = next(row for row in view["bot_settings"]["schedules"] if row["name"] == "Ping")
    require(saved["first_run_at_ms"] == first_run and saved["enabled"] and saved["next_run_at_ms"] is not None,
            "the conversation lists the shared skill and its schedule with the next run")

    prefix = "schedule:" + saved["schedule_id"] + ":"
    replies = wait_for("the schedule runs at its first time", lambda: replies_to(prefix), timeout=60)
    deadline = time.monotonic() + 32
    while time.monotonic() < deadline:
        require(len(replies_to(prefix)) == 1, "one schedule time runs exactly once")
        time.sleep(1)
    rejected = results(replies[0]["in_reply_to"], "update_profile")
    require(rejected and not rejected[0]["accepted"], "a schedule run cannot change the bot")
    receipt = send("bot-cleanup", json.dumps({"chat_agent": True, "tools": [
        {"name": "remove_schedule", "arguments": {"call_ref": "schedule-off", "schedule_id": saved["schedule_id"]}},
        {"name": "read_skill", "arguments": {"skill_id": skill["skill_id"]}},
    ]}))
    require(results(receipt["trigger"], "read_skill")[0]["result"]["body"] == "List yesterday's commits by author."
            and snapshot()["bot_settings"]["schedules"] == [], "a removed schedule stops and a shared skill reads back")
    send("bot-rename", "rename yourself to Orion")
    require(snapshot()["bot"]["name"] == "Orion" and len(snapshot()["changes"]) == 5,
            "the user's plain request renames the bot")
    autonomous = [row for row in fixture_records(run, "manager-tools.jsonl")
                  if row["name"] == "update_profile" and row["trigger"].startswith("work:")]
    require(autonomous and not any(row["accepted"] for row in autonomous),
            "a work result turn cannot change the bot")
    passed("the user's own message changes the bot once per call_ref and a schedule runs once per time",
           changes=len(snapshot()["changes"]), schedule=saved["schedule_id"])

    paused_at = int(time.time() * 1000) + 15000
    receipt = send("agent-capabilities", json.dumps({"chat_agent": True, "tools": [
        {"name": "remind", "arguments": {"call_ref": "remind", "note": "Check the deploy again.", "in_minutes": 1}},
        {"name": "remind", "arguments": {"call_ref": "remind", "note": "Check the deploy again.", "in_minutes": 1}},
        {"name": "save_schedule", "arguments": {"call_ref": "paused", "name": "Paused", "prompt": "Say the paused schedule ran.",
                                                "first_run_at_ms": paused_at, "every_minutes": 5}},
        {"name": "remember", "arguments": {"call_ref": "note", "text": "The user prefers short replies."}},
        {"name": "remember", "arguments": {"call_ref": "secret", "text": "api key sk-" + "a" * 40}, "accepted": False,
         "rejection": "credential"},
        {"name": "fetch_url", "arguments": {"url": "http://127.0.0.1:1/"}, "accepted": False,
         "rejection": "local or private network"},
        {"name": "fetch_url", "arguments": {"url": "http://192.168.1.1/"}, "accepted": False,
         "rejection": "local or private network"},
        {"name": "fetch_url", "arguments": {"url": "http://[::1]/"}, "accepted": False,
         "rejection": "local or private network"},
        {"name": "fetch_url", "arguments": {"url": "file:///etc/hosts"}, "accepted": False, "rejection": "only http and https"},
        {"name": "list_bots", "arguments": {}},
        {"name": "message_bot", "arguments": {"call_ref": "ask-beta", "bot": "Beta", "text": "Please check the build."}},
        {"name": "message_bot", "arguments": {"call_ref": "ask-beta", "bot": "Beta", "text": "Please check the build."}},
    ]}))
    reminder = results(receipt["trigger"], "remind")
    require(reminder[0]["result"] == reminder[1]["result"], "a retried reminder returns the one recorded reminder")
    reminder_id = reminder[0]["result"]["change"]["schedule_id"]
    paused = next(row for row in snapshot()["bot_settings"]["schedules"] if row["name"] == "Paused")
    a.client.rpc("control_schedule", conversation_id=name, schedule_id=paused["schedule_id"], action="pause")
    require(not next(row for row in snapshot()["bot_settings"]["schedules"] if row["name"] == "Paused")["enabled"],
            "the user's signed pause shows on the schedule")
    a.client.rpc("control_schedule", unsigned=True, conversation_id=name, schedule_id=paused["schedule_id"],
                 action="resume", status=400)
    require({"Beta", "Orion"} <= {row["name"] for row in results(receipt["trigger"], "list_bots")[0]["result"]["bots"]},
            "list_bots lists the user's bots")
    passed("fetch_url refuses loopback, private and non-web addresses and notes refuse secrets",
           refusals=len([row for row in fixture_records(run, "manager-tools.jsonl")
                         if row["trigger"] == receipt["trigger"] and not row["accepted"]]))

    sent = results(receipt["trigger"], "message_bot")
    require(sent[0]["result"]["message"] == sent[1]["result"]["message"], "a retried bot message is one message")
    bot_trigger = "bot_message:" + sent[0]["result"]["message"]["message_id"]
    wait_for("the other bot answers the bot message", lambda:
             (scheduler_record(a, network, bot_trigger) or {}).get("state") == "completed")
    woken = [row for row in fixture_records(run) if row["trigger"] == bot_trigger]
    beta_view = snapshot("bot-history-beta")
    shown = [message for message in beta_view["messages"] if message["role"] == "bot"]
    require(len(woken) == 1 and woken[0]["kind"] == "bot_message" and woken[0]["conversation_id"] == "bot-history-beta"
            and len(shown) == 1 and shown[0]["sender"]["name"] == "Orion",
            "a bot message wakes the other bot once and shows who sent it")
    rejected = results(bot_trigger, "update_profile")
    require(rejected and not rejected[0]["accepted"], "a bot message turn cannot change the bot")
    passed("message_bot wakes the other bot exactly once per call_ref", trigger=bot_trigger)

    reminder_prefix = "schedule:" + reminder_id + ":"
    wait_for("the reminder fires once", lambda: replies_to(reminder_prefix), timeout=90)
    require(not replies_to("schedule:" + paused["schedule_id"] + ":"), "a paused schedule does not run")
    fired = lambda: [row for row in fixture_records(run) if row["trigger"].startswith("schedule:")]
    before = len(fired())
    a.stop()
    a.start()
    a.client.connect()
    a.client.authenticate()
    require_quiet("a restart never replays a reminder", lambda: len(fired()) == before)
    view = snapshot()
    require(len(replies_to(reminder_prefix)) == 1 and all(row["schedule_id"] != reminder_id for row in view["bot_settings"]["schedules"]),
            "a fired reminder stays fired after restart")
    require([row["text"] for row in view["bot_settings"]["memory"]] == ["The user prefers short replies."],
            "a note survives the restart")
    a.client.rpc("control_schedule", conversation_id=name, schedule_id=paused["schedule_id"], action="delete")
    require(all(row["schedule_id"] != paused["schedule_id"] for row in snapshot()["bot_settings"]["schedules"])
            and not replies_to("schedule:" + paused["schedule_id"] + ":"), "a deleted schedule is gone without running")
    passed("a reminder fires once and never again after restart, a paused schedule is honored and notes persist",
           reminder=reminder_id, paused=paused["schedule_id"])


def chat_agent_acceptance(options):
    binary = options.binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("chat-agent-process-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir()
    codex = fixture_wrapper(run, "fixture-codex", "--fixture-codex")
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    nodes = []
    report = {"status": "running", "source_binary": str(binary), "binary_sha256": file_sha256(binary),
              "provider_calls": 0, "coverage": "one real Node; fixture SDK driving the real vibeface CLIs against a managed fixture App Server",
              "evidence": [], "not_checked": ["real provider model behavior", "rendered browser UI",
                  "cross-device work (phase 2)", "OS sandboxing of SDK host tools"]}

    def passed(description, **evidence):
        report["evidence"].append({"check": description, **evidence})
        print("PASS " + description, flush=True)

    try:
        for name in ("A", "BOT-OTHER-NETWORK"):
            node = Node(run, name, binary, workspace, helper)
            node.configure(timeout_secs=600, manager=fixture_manager_settings(run, workspace) if name == "A" else None,
                           bins={"codex": str(codex) if name == "A" else sys.executable}, app_server=SPAWN_APP_SERVER,
                           daily_decision_limit=100)
            nodes.append(node)
        a, outsider = nodes
        a.start()
        network = a.bootstrap()
        outsider.start()
        outsider.bootstrap()
        schema = fixture_schema_check()
        passed("the fake App Server answers every client request in the pinned schema",
               client_requests=len(schema["client_requests"]))
        bot_checks(run, workspace, a, network, passed, outsider)
        work_checks(run, workspace, a, network, passed)
        triage_checks(run, workspace, a, network, passed)
        foreign_session_checks(run, workspace, a, network, passed)
        bot_settings_checks(run, a, network, passed)
        triage_budget_checks(run, workspace, a, network, passed)
        report["manager_invocations"] = len(fixture_records(run))
        report["triage_invocations"] = len(fixture_records(run, "triage-invocations.jsonl"))
        no_agent_checks(run, a, passed)
        report["dag"] = verify_dag(a, network)
        passed("the signed per-device log stays contiguous after work, decisions and crash recovery", **report["dag"])
        report["status"] = "passed"
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        cleanup_errors = []
        for node in reversed(nodes):
            for operation, cleanup in (("client.close", node.client.close), ("node.stop", node.stop)):
                try:
                    cleanup()
                except BaseException as error:
                    cleanup_errors.append({"node": node.name, "operation": operation,
                                           "type": type(error).__name__, "message": str(error)})
                    if operation == "node.stop" and node.process is not None:
                        try:
                            node.stop(crash=True)
                        except BaseException as error:
                            cleanup_errors.append({"node": node.name, "operation": "node.stop(crash=True)",
                                                   "type": type(error).__name__, "message": str(error)})
        try:
            stop_fixture_processes(run)
        except BaseException as error:
            cleanup_errors.append({"operation": "stop_fixture_processes",
                                   "type": type(error).__name__, "message": str(error)})
        if cleanup_errors:
            report["status"] = "failed"
            report["cleanup_errors"] = cleanup_errors
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)
        if cleanup_errors and "failure" not in report:
            raise RuntimeError("fixture cleanup failed; see " + str(run / "report.json"))


def verify_dag(node, network):
    """Each device's main log is contiguous, heads match it and every stored event's parents are stored."""
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        rows = connection.execute("SELECT author, seq, hash, lamport, envelope, main FROM events WHERE network_id=? "
                                  "ORDER BY main DESC, author, seq", (network,)).fetchall()
        heads = dict(((author, (seq, head)) for author, seq, head in connection.execute(
            "SELECT author, max_seq, head_hash FROM log_heads WHERE network_id=?", (network,))))
        pending = connection.execute("SELECT COUNT(*) FROM pending_events WHERE network_id=?", (network,)).fetchone()[0]
    lamport = {row[2]: row[3] for row in rows}
    previous, forks = {}, 0
    for author, seq, event_hash, clock, envelope, main in rows:
        event = json.loads(json.loads(envelope)["canonical_body"])
        require(event["author"] == author and event["seq"] == seq, "stored columns match the signed body")
        parents = event["deps"] + ([event["prev"]] if event["prev"] else [])
        require(event["deps"] == sorted(event["deps"]) and all(parent in lamport for parent in parents),
                "dependencies are sorted and every parent is stored")
        require(all(clock > lamport[parent] for parent in parents), "lamport clock exceeds every causal parent")
        if not main:
            forks += 1
            continue
        expected_seq, expected_prev = previous.get(author, (0, None))
        require(seq == expected_seq + 1 and event["prev"] == expected_prev,
                "each author's main log is contiguous and chained by prev")
        previous[author] = (seq, event_hash)
    require(heads == previous, "log_heads equals the last event of every author's main log")
    require(pending == 0 and forks == 0, "a single Node keeps no pending or forked events")
    return {"events": len(rows), "authors": len(previous), "heads": {author: seq for author, (seq, _) in previous.items()}}


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
    codex = fixture_wrapper(run, "fixture-codex", "--fixture-codex")
    manager = fixture_manager_settings(run, workspace)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    nodes = []
    report = {"status": "running", "source_binary": str(source_binary),
              "binary_sha256": file_sha256(binary), "provider_calls": 0,
              "coverage": "one real Node, durable Scheduler and signed log; fixture SDK",
              "evidence": [], "not_checked": ["real Cursor/Codex model success",
                  "rendered browser UI", "cross-device delivery (phase 2)",
                  "instruction-boundary claim/final-commit crash injection (verify separately in Rust tests)"]}

    def passed(description, **evidence):
        report["evidence"].append({"check": description, **evidence})
        print("PASS " + description, flush=True)

    def calls(conversation=None, *, label=None):
        return [record for record in fixture_records(run)
                if (conversation is None or record["conversation_id"] == conversation)
                and (label is None or record["label"] == label)]

    def conversation(name):
        return a.client.rpc("conversation", conversation_id=name)

    def new_conversation(name):
        a.client.rpc("create_conversation", conversation_id=name, title=name)
        return name

    def send(name, label, **directive):
        text = json.dumps({"phase1": True, "label": label, **directive})
        receipt = a.client.rpc("message", conversation_id=name, message_id=label, text=text,
                               report_results=directive.get("report_results", False))
        require(receipt["accepted"] is True and receipt["message_id"] == label
                and receipt["trigger"] == "message:" + label, "acceptance pins the message trigger")
        proof = receipt["receipt"]
        require(proof["network_id"] == network
                and proof["conversation_id"] == name and proof["trigger"] == receipt["trigger"],
                "durable receipt has the exact accepted identity")
        return receipt, text

    def state(receipt, expected):
        record = scheduler_record(a, network, receipt["trigger"])
        return record if record and record["state"] == expected else None

    def completed(receipt):
        record = scheduler_record(a, network, receipt["trigger"])
        if record and record["state"] in {"failed", "interrupted", "cancelled"}:
            raise RuntimeError("management job reached " + record["state"] + ": " + record.get("detail", ""))
        return record if record and record["state"] == "completed" else None

    def reply(name, job):
        return [message for message in conversation(name)["messages"] if message["message_id"] == job["message_id"]]

    def reconnect():
        a.client.connect()
        wait_for("A receives its client grant", lambda: a.client.authenticate())

    try:
        a = Node(run, "A", binary, workspace, helper)
        nodes.append(a)
        a.configure(timeout_secs=600, manager=manager, bins={"codex": str(codex)}, app_server=SPAWN_APP_SERVER,
                    daily_decision_limit=100)
        a.start()
        network = a.bootstrap()
        a.client.call("subscribe")
        passed("one real Node bootstraps; the Manager uses a controlled protocol fixture")

        name = new_conversation("phase1-queue")
        gate = run / "release-first"
        first, original = send(name, "queue-first", gate=str(gate))
        require(first["state"] == "queued", "local receipt confirms durable queue admission")
        wait_for("first management invocation starts", lambda: len(calls(name, label="queue-first")) == 1)
        second, _ = send(name, "queue-second")
        wait_for("busy second message remains durably queued", lambda: state(second, "queued"))
        cancelled_queued, _ = send(name, "queue-cancelled")
        queued_before = wait_for("cancellation target remains queued behind the active reply",
                                 lambda: state(cancelled_queued, "queued"))
        retry = a.client.rpc("message", conversation_id=name, message_id="queue-first", text=original,
                             report_results=False)
        require(retry["trigger"] == first["trigger"], "message retry retains its exact trigger")
        a.client.rpc("message", conversation_id=name, message_id="queue-first", text="changed", status=400)
        progress = wait_for("the client observes actual management text", lambda:
            [event["payload"] for event in a.client.command("events") if event.get("payload", {}).get("type") == "manager_progress"
             and event["payload"].get("conversation_id") == name and event["payload"].get("trigger") == first["trigger"]])
        require(all(row["source_node"] == a.device_id and row["invocation_id"] and row["sequence"] > 0
                    and row["progress"]["type"] == "text_snapshot" for row in progress),
                "progress carries the invocation identity and normalized text snapshots")
        pushed = wait_for("the client receives the posted message as records, not a reload", lambda:
            [event["payload"] for event in a.client.command("events") if event.get("payload", {}).get("type") == "records"
             and event["payload"]["conversation_id"] == name
             and any(row["message_id"] == "queue-first" and row["rank"] for row in event["payload"]["messages"])])
        require(all(isinstance(row["jobs"], list) and isinstance(row["works"], list) for row in pushed),
                "pushed records carry the conversation's jobs and changed works")
        active_before = state(first, "running")
        require(active_before, "original reply remains running before queued cancellation")
        cancelled_reply = a.client.rpc("cancel_manager", conversation_id=name, trigger=cancelled_queued["trigger"])
        require(cancelled_reply.get("cancel_requested") is True and cancelled_reply["job"]["state"] == "cancelled"
                and "attempt" not in cancelled_reply["job"], "queued work is cancelled before any SDK attempt")
        cancelled_job = state(cancelled_queued, "cancelled")
        require(all(cancelled_job[key] == queued_before[key] for key in ("message_id", "basis", "sequence")),
                "queued cancellation preserves the original accepted message and basis")
        repeated = a.client.rpc("cancel_manager", conversation_id=name, trigger=cancelled_queued["trigger"])
        require(repeated["job"] == cancelled_reply["job"], "repeated queued cancellation confirms the same terminal job")
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
            race, _ = send(name, "consumer-rewake-" + str(number))
            wait_for("consumer exit/new enqueue is not lost", lambda: completed(race))
        snapshot = conversation(name)
        jobs = snapshot["scheduler"]["jobs"]
        completed_jobs = [job for job in jobs if job["state"] == "completed"]
        require(len(jobs) == 7 and len(completed_jobs) == len(calls(name)) == 6
                and {job["trigger"] for job in completed_jobs} == {call["trigger"] for call in calls(name)},
                "each uncancelled trigger invokes once and queued cancellation remains terminal")
        replies = [message for message in snapshot["messages"] if message["role"] == "assistant"]
        stopped_replies = [message for message in replies if message.get("in_reply_to") == "queue-cancelled"]
        require(len(stopped_replies) == 1 and stopped_replies[0]["status"] == "stopped"
                and stopped_replies[0]["message_id"] == cancelled_job["message_id"]
                and sum(message["message_id"] == "queue-cancelled" for message in snapshot["messages"]) == 1,
                "cancelled queued work keeps its user message and gets one stopped reply without an SDK call")
        replies = [message for message in replies if message not in stopped_replies]
        require(len(replies) == 6 and all(sum(message["message_id"] == job["reply_message_id"] for message in replies) == 1
                and next(message for message in replies if message["message_id"] == job["reply_message_id"])["in_reply_to"]
                == job["message_id"] for job in completed_jobs),
                "each completed job has exactly one bot reply pointing at its message")
        require(calls(name)[1]["resumed"] and calls(name)[0]["native_session_id"] == calls(name)[1]["native_session_id"],
                "consecutive calls preserve the local native session")
        passed("busy messages persist, retry deduplicates, queued cancellation is final and replies match",
               triggers=[first["trigger"], second["trigger"]], invocations=len(calls(name)))

        name = new_conversation("phase1-steer")
        gate = run / "release-steer"
        first, _ = send(name, "steer-first", gate=str(gate), report_results=True)
        wait_for("the reply runs on its App Server turn", lambda: state(first, "running") and calls(name))
        joined, joined_text = send(name, "steer-joined", report_results=True)
        def steers():
            return [row["message"] for row in map(json.loads, (run / "codex-rpc.jsonl").read_text().splitlines())
                    if row["message"].get("method") == "turn/steer"
                    and row["message"]["params"].get("clientUserMessageId") == "steer-joined"]
        steering = wait_for("a message sent mid-reply joins the running turn",
                            lambda: state(joined, "steering") and steers() and state(joined, "steering"))
        into = json.loads(steering["detail"])["into"]
        retry = a.client.rpc("message", conversation_id=name, message_id="steer-joined", text=joined_text,
                             report_results=True)
        require(retry["trigger"] == joined["trigger"], "a retried message keeps its trigger")
        gate.touch()
        done = wait_for("the reply that took the message completes", lambda: completed(first))
        answer = reply(name, done)
        merged = wait_for("the joined message settles once its reply completed", lambda: state(joined, "steered"))
        require(json.loads(merged["detail"])["outcome"] == "answered", "the joined message was answered by that reply")
        require(len(steers()) == 1 and steers()[0]["params"]["expectedTurnId"] == into["turn_id"],
                "the message is steered once into the running turn, whatever the retries")
        require(len(answer) == 1 and answer[0]["text"].endswith("+steer-joined"),
                "the running turn's one reply answers the joined message")
        require(len(calls(name)) == 1 and state(joined, "steered")
                and not any(message.get("in_reply_to") == "steer-joined" for message in conversation(name)["messages"]),
                "a joined message gets no turn or reply of its own")
        passed("a message sent while the bot replies joins that reply through one turn/steer",
               trigger=joined["trigger"], turn=into["turn_id"])

        for unconfirmed in (False, True):
            name = new_conversation("phase1-cancel-" + str(unconfirmed).lower())
            cancelled, _ = send(name, "cancel-" + str(unconfirmed).lower(),
                                gate=str(run / "never-release-cancel"), cancel_unknown=unconfirmed)
            wait_for("controlled management call starts before cancel", lambda: calls(name))
            response = a.client.rpc("cancel_manager", conversation_id=name, trigger=cancelled["trigger"])
            require(response.get("cancel_requested") is True, "cancel request has an explicit pending state")
            terminal = "interrupted" if unconfirmed else "cancelled"
            stopped = wait_for("cancel reflects actual confirmation evidence", lambda: state(cancelled, terminal))
            require(len(calls(name)) == 1, "cancel does not restart the same management trigger")
            answer = reply(name, stopped)
            require(len(answer) == 1 and answer[0]["status"] == ("interrupted" if unconfirmed else "stopped")
                    and answer[0]["in_reply_to"] == cancelled["message_id"],
                    "the stopped turn becomes one bot message marked with its status")
            wait_for("the Node settles the exited process without a card", lambda: not conversation(name)["checks"])
            require(state(cancelled, terminal) and len(calls(name)) == 1,
                    "reconciliation neither changes nor replays the stopped job")
            passed("management stop becomes " + terminal + " with a " + answer[0]["status"] + " reply",
                   trigger=cancelled["trigger"])

        name = new_conversation("phase1-restart")
        interrupted, _ = send(name, "restart-running", gate=str(run / "never-release"))
        wait_for("old attempt crosses running boundary", lambda: state(interrupted, "running") and calls(name))
        queued, _ = send(name, "restart-queued")
        wait_for("second trigger is persisted before crash", lambda: state(queued, "queued"))
        old_attempt = state(interrupted, "running")["attempt"]["id"]
        a.stop(crash=True)
        stop_fixture_processes(run)
        a.start()
        reconnect()
        stopped = wait_for("restarted running attempt becomes interrupted", lambda: state(interrupted, "interrupted"))
        wait_for("queued trigger performs its first call after restart", lambda: completed(queued))
        require(stopped["attempt"]["id"] == old_attempt and len(calls(name, label="restart-running")) == 1
                and len(calls(name, label="restart-queued")) == 1, "recovery keeps the original attempt and never replays it")
        wait_for("the Node settles the crashed attempt's exited processes", lambda: not conversation(name)["checks"])
        require(state(interrupted, "interrupted") and len(calls(name, label="restart-running")) == 1,
                "reconciliation neither changes nor replays the original interrupted job")
        passed("a crash interrupts the running attempt, reconciles its processes and never replays it",
               attempt_id=old_attempt)

        name = new_conversation("phase1-budget")
        day = time.strftime("%Y-%m-%d", time.gmtime())
        with sqlite3.connect(a.directory / "node.sqlite3") as connection:
            connection.execute("INSERT INTO budget_days(network_id,day,used) VALUES(?,?,?) "
                               "ON CONFLICT(network_id,day) DO UPDATE SET used=excluded.used", (network, day, 100))
        human, _ = send(name, "human-after-autonomous-budget")
        wait_for("human chat ignores exhausted autonomous budget", lambda: completed(human))
        with sqlite3.connect(f"file:{a.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
            used = connection.execute("SELECT used FROM budget_days WHERE network_id=? AND day=?", (network, day)).fetchone()[0]
        require(used == 100, "human chat neither consumes nor resets autonomous usage")
        passed("human chat remains available without consuming exhausted autonomous budget")

        before = len(calls())
        a.stop()
        a.start()
        reconnect()
        require_quiet("a restart never re-runs finished jobs from the signed history", lambda: len(calls()) == before)
        dag = verify_dag(a, network)
        passed("the signed per-device log is contiguous, chained and matches log_heads", **dag)
        report["status"] = "passed"
        report["manager_invocations"] = len(calls())
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        stop_fixture_processes(run)
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


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
        node = Node(run, "WEB", binary, workspace, None)
        node.configure(app_server={"mode": "system", "socket": str(run / "unavailable.sock")})
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


def heads(node, network):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        return dict(connection.execute("SELECT author, max_seq FROM log_heads WHERE network_id=?", (network,)))


def head(node, network, author):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        return connection.execute("SELECT max_seq, head_hash FROM log_heads WHERE network_id=? AND author=?",
                                  (network, author)).fetchone()


def converged(nodes, network):
    """Every Node holds the same per-author heads and a contiguous, fully linked log."""
    first = heads(nodes[0], network)
    return first if first and all(heads(node, network) == first for node in nodes[1:]) else None


def gateway_refusal(client, device_id):
    """The text a Gateway sends instead of a Noise channel when it will not forward to `device_id`."""
    refused = client.connect(device_id, check=False)
    require(not refused["ok"], "the Gateway refuses this target")
    return refused["error"]


def acceptance(options):
    binary, module = options.binary.resolve(strict=True), options.crypto_module.resolve(strict=True)
    require(module.with_name("vibeface_secure_bg.wasm").read_bytes().startswith(b"\x00asm"),
            "packaged browser WASM is available")
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("node-acceptance-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    nodes, clients = [], []
    report = {"status": "running", "cases": {}, "provider_calls": 0, "binary_sha256": file_sha256(binary),
              "coverage": "four real Node processes over iroh with a member relay; WASM browser client; fixture SDK",
              "not_checked": ["real NAT and public ingress (disjoint loopback IP families require the relay)",
                              "attachment resume: no product path writes attachments yet; the Rust test "
                              "member_nodes_sync_ranges_and_attachments_without_jobs_and_refuse_outsiders resumes 3 MiB",
                              "non-member endpoints refused by sync and RPC (same Rust test)",
                              "rendered browser UI", "provider models"]}

    def passed(name, **evidence):
        report["cases"][name] = evidence
        print("PASS " + name, flush=True)

    def browser(node, manager):
        """The manager's browser opening another member Gateway: same device key, another entrance."""
        client = Client(node.url, helper, identity_file=manager.identity_file)
        client.network_id = manager.network_id
        clients.append(client)
        return client

    def signed_message(client, conversation, message_id, text):
        body = {"conversation_id": conversation, "message_id": message_id, "text": text, "report_results": False}
        return {"method": "message", **body, "attestation": client.attestation("message", dict(body))}

    try:
        relay_port = available_port()
        relay_url = f"http://127.0.0.1:{relay_port}"
        for name in ("A", "P", "B", "C", "OTHER-NETWORK"):
            nodes.append(Node(run, name, binary, workspace, helper))
        a, p, b, c, outsider = nodes
        a.configure(manager=fixture_manager_settings(run, workspace), bins={"codex": str(fixture_wrapper(run, "fixture-codex", "--fixture-codex"))},
                    app_server=SPAWN_APP_SERVER, daily_decision_limit=100)
        p.configure_network(relay_listen=f"127.0.0.1:{relay_port}")
        a.configure_network(relays=[relay_url])
        c.configure_network(relays=[relay_url])
        b.configure_network(relays=[relay_url], bind=f"[::1]:{b.port}")
        a.start()
        network = a.bootstrap()
        p.pair(a)
        p.start()
        at_p = browser(p, a.client)
        at_p.connect(p.device_id)
        wait_for("the manager's browser is known at P", lambda: at_p.authenticate())
        b.pair(at_p)
        c.pair(a)
        for node in (b, c):
            node.start()
        conversation = "p3-conversation"
        a.client.rpc("create_conversation", conversation_id=conversation, title="Phase 3 acceptance")
        a.client.call("rpc", signed_message(a.client, conversation, "m1", "first message from A"))
        members = (a, p, b, c)
        synced = wait_for("A, P, B (IPv6-only) and C converge", lambda: converged(members, network), timeout=60)
        dags = {node.name: verify_dag(node, network)["events"] for node in members}
        require(all(scheduler_records(node, network) == [] for node in (p, b, c)),
                "syncing history creates no scheduler job on replicas")
        b_config = json.loads((b.directory / "config.json").read_text())["network"]
        require(b_config["bind"] == f"[::1]:{b.port}" and b_config["relays"] == [relay_url], "B has no IP family in common with the IPv4-only peers")
        require(wait_for("B actually selects a relay data path", lambda: any(
            row.get("path") == "relay" for row in node_status(b).get("peers", []))), "B uses a real relay path")
        passed("join-and-sync-over-iroh", heads=synced, events=dags, relay=relay_url)

        entrances = a.client.call("entrances")["entrances"]
        opened = [json.loads(item["payload"]) for item in entrances]
        require({item["device_id"] for item in opened} == {node.device_id for node in members},
                "every member Gateway publishes one signed entrance record")
        require(all(a.client.command("verify_address", device_id=json.loads(item["payload"])["device_id"],
                                     payload=item["payload"], signature=item["signature"]) for item in entrances),
                "the browser WASM verifies each record against its own Node key")
        forged = dict(entrances[0])
        forged["payload"] = forged["payload"].replace(json.loads(forged["payload"])["gateway_url"], "https://forged.example")
        require(not a.client.command("verify_address", device_id=json.loads(forged["payload"])["device_id"],
                                     payload=forged["payload"], signature=forged["signature"]),
                "a record whose gateway_url was changed fails verification")
        by_device = {item["device_id"]: item for item in opened}
        require(by_device[p.device_id]["gateway_url"] == p.url, "P's record names its own Gateway URL")
        b_paths = by_device[b.device_id]["endpoint"]["addrs"]
        require([address["Relay"].rstrip("/") for address in b_paths if "Relay" in address] == [relay_url],
                "B advertises exactly P's relay")
        require([address["Ip"] for address in b_paths if "Ip" in address] == [f"[::1]:{b.port}"],
                "B advertises its configured IPv6-only direct candidate")
        require(all(addr["Ip"].startswith("127.0.0.1:") for node in (a, p, c)
                    for addr in by_device[node.device_id]["endpoint"]["addrs"] if "Ip" in addr),
                "Nodes advertise only the loopback address they were told to bind")
        passed("signed-entrances", count=len(entrances), gateways=sorted(item["gateway_url"] for item in opened),
               endpoints={node.name: by_device[node.device_id]["endpoint"]["addrs"] for node in members})

        at_b = browser(b, a.client)
        at_b.connect(b.device_id)
        at_b.authenticate()
        wait_for("B learns a relay path for every Node", lambda: all(
            any("Relay" in addr for addr in json.loads(item["payload"])["endpoint"]["addrs"])
            for item in at_b.call("entrances")["entrances"]), timeout=60)
        reached = {}
        for gateway, target in ((p, a), (p, b), (c, b), (b, a)):
            client = browser(gateway, a.client)
            opened = client.connect(target.device_id, check=False)
            require(opened["ok"], f"{gateway.name} forwards the browser to {target.name}: {opened.get('error')}")
            client.authenticate()
            require(client.call("info")["device_id"] == target.device_id, "Noise ends at the target, not the Gateway")
            history = client.rpc("conversation", conversation_id=conversation)
            require(any(row["message_id"] == "m1" for row in history["messages"]), "the target shows the synced history")
            reached[f"{gateway.name}->{target.name}"] = True
            client.close()
        outsider.start()
        outsider.bootstrap()
        refusal = gateway_refusal(browser(p, a.client), outsider.device_id)
        require("target_not_member" in refusal, "a Gateway refuses a target outside its personal network")
        require(upgrade_status(c, "/api/client", p.url) == 101, "C accepts the Origin of member Gateway P")
        require(upgrade_status(c, "/api/client", "https://elsewhere.example") == 403, "other origins stay refused")
        passed("cross-entrance-forwarding", reached=sorted(reached), refusal="target_not_member")

        marker = workspace / "delivery-count.txt"
        directive = json.dumps({"label": "delivery", "host_command": "printf once >> delivery-count.txt",
                                "host_cwd": str(workspace)})
        at_c = browser(c, a.client)
        at_c.connect(c.device_id)
        at_c.authenticate()
        a.stop()
        request = signed_message(at_c, conversation, "m2-delivered", directive)
        receipt = at_c.call("rpc", request)
        require(receipt["state"] == "pending_delivery", "C keeps the message for its scheduling Node A")
        require_quiet("nothing runs while A is down", lambda: not marker.exists() and not invocations(run, "delivery"))
        a.start()
        wait_for("A admits C's delivery and runs it once", lambda:
                 marker.exists() and marker.read_text() == "once" and len(invocations(run, "delivery")) == 1, timeout=60)
        at_b.connect(b.device_id)
        at_b.authenticate()
        again = at_b.call("rpc", request)
        require(again["trigger"] == receipt["trigger"], "the same signed request through B answers the original trigger")
        c.stop(crash=True)
        a.stop()
        a.start()
        c.start()
        a.client.connect()
        a.client.authenticate()
        wait_for("A and C converge after a crash and a restart", lambda: converged(members, network), timeout=60)
        require_quiet("delivery, retry through B, crash and restart never repeat the host write", lambda:
                      marker.read_text() == "once" and len(invocations(run, "delivery")) == 1)
        job = scheduler_record(a, network, receipt["trigger"])
        require(job and job["state"] == "completed", "A recorded one completed job for the delivered message")
        require(all(scheduler_records(node, network) == [] for node in (p, b, c)), "replicas still hold no jobs")
        passed("delivery-without-replay", trigger=receipt["trigger"], invocations=1)

        p.stop()
        a.client.call("rpc", signed_message(a.client, conversation, "m3", "written while the relay is down"))
        wait_for("C still syncs directly while P is down", lambda: heads(c, network) == heads(a, network))
        require_quiet("B (IPv6-only) cannot sync while P is down", lambda: heads(b, network) != heads(a, network))
        p.start()
        healed = wait_for("B catches up once P's relay returns", lambda: converged(members, network), timeout=90)
        passed("relay-outage-and-recovery", heads=healed)

        c.stop()
        failover = browser(p, a.client)
        failover.connect(a.device_id)
        failover.authenticate()
        late = signed_message(failover, conversation, "m4", "sent after the C entrance closed")
        first = failover.call("rpc", late)
        require(failover.call("rpc", late)["trigger"] == first["trigger"], "resending through the new entrance is idempotent")
        c.start()
        final = wait_for("all four Nodes converge after the entrance switch", lambda: converged(members, network), timeout=60)
        shown = [row["message_id"] for row in failover.rpc("conversation", conversation_id=conversation)["messages"]
                 if row["role"] == "user"]
        require(sorted(shown) == ["m1", "m2-delivered", "m3", "m4"], "each message appears once after failover")
        passed("entrance-switch", heads=final, messages=shown)

        at_c = browser(c, a.client)
        at_c.connect(c.device_id)
        at_c.authenticate()
        at_c.call("subscribe")
        directive = json.dumps({"label": "remote-stop", "gate": str(run / "never-release-remote-stop")})
        held = at_c.call("rpc", signed_message(at_c, conversation, "m5-stop", directive))

        def owner_job(state):
            view = at_c.rpc("conversation", conversation_id=conversation)
            job = next((row for row in view["scheduler"]["jobs"] if row["trigger"] == held["trigger"]), None)
            return view if job and job["state"] == state and job.get("source_node") == a.device_id \
                and view["scheduler"]["remote"] == {"owner": a.device_id, "reachable": True} else None
        view = wait_for("C shows A's running reply", lambda: owner_job("running"), timeout=60)
        require(view["observation_targets"] == [a.device_id] and view["scheduler"]["capabilities"]["cancel"] is True,
                "C follows A's progress and offers A's stop")
        wait_for("C receives the text A's reply streams", lambda: [
            event for event in at_c.command("events") if event.get("payload", {}).get("type") == "manager_progress"
            and event["payload"].get("trigger") == held["trigger"] and event["payload"].get("source_node") == a.device_id])
        receipt = at_c.rpc("cancel_manager", conversation_id=conversation, trigger=held["trigger"])
        require(receipt.get("cancel_requested") is True and receipt.get("owner") == a.device_id, "C's stop is carried out by A")
        wait_for("A stops its reply", lambda: (scheduler_record(a, network, held["trigger"]) or {}).get("state") == "cancelled")
        stopped = wait_for("C shows the stopped reply", lambda: [
            row for row in at_c.rpc("conversation", conversation_id=conversation)["messages"]
            if row.get("in_reply_to") == "m5-stop" and row.get("status") == "stopped"])
        require(sum(row["label"] == "remote-stop" for row in fixture_records(run)) == 1 and scheduler_records(c, network) == [],
                "the reply ran once on A and C stored none of its jobs")
        a.stop()
        unconfirmed = at_c.rpc("cancel_manager", conversation_id=conversation, trigger=held["trigger"])
        require(unconfirmed == {"cancel_requested": False, "unconfirmed": True, "owner": a.device_id},
                "a stop A does not answer is unconfirmed")
        away = at_c.rpc("conversation", conversation_id=conversation)["scheduler"]
        require(away["remote"] == {"owner": a.device_id, "reachable": False}
                and not any(row.get("source_node") for row in away["jobs"]), "C shows A's state as unknown, not stale")
        a.start()
        a.client.connect()
        a.client.authenticate()
        wait_for("the Nodes converge after A returns", lambda: converged(members, network), timeout=60)
        failover.connect(p.device_id)
        failover.authenticate()
        require((scheduler_record(a, network, held["trigger"]) or {}).get("state") == "cancelled"
                and sum(row["label"] == "remote-stop" for row in fixture_records(run)) == 1, "the unconfirmed stop changed nothing on A")
        passed("cross-device-stop", trigger=held["trigger"], reply=stopped[0]["message_id"])

        a.client.revoke(c.device_id, head(a, network, c.device_id))
        wait_for("P drops C's entrance after the revocation", lambda: all(
            json.loads(item["payload"])["device_id"] != c.device_id
            for item in failover.call("entrances")["entrances"]))
        require("target_not_member" in gateway_refusal(browser(p, a.client), c.device_id),
                "Gateways stop forwarding to a revoked Node")
        passed("revoked-entrance")
        require(file_sha256(binary) == report["binary_sha256"], "one immutable binary completes acceptance")
        report["status"] = "passed"
    except BaseException as error:
        report.update(status="failed", failure=f"{type(error).__name__}: {error}", traceback=traceback.format_exc())
        raise
    finally:
        for client in clients:
            client.close()
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        stop_fixture_processes(run)
        report["cleanup"] = {"remaining_nodes": sum(node.process is not None for node in nodes)}
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)

def scope_events(node, network, scope):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        rows = connection.execute("SELECT envelope FROM events WHERE network_id=? ORDER BY lamport, hash", (network,)).fetchall()
    events = [json.loads(json.loads(row[0])["canonical_body"]) for row in rows]
    return [event for event in events if event["scope_id"] == scope]


def committed_blob_offset(node, network, digest):
    with sqlite3.connect(f"file:{node.directory / 'node.sqlite3'}?mode=ro", uri=True) as connection:
        row = connection.execute("SELECT committed FROM blob_offsets WHERE hash=?", (digest,)).fetchone()
    return row[0] if row else 0


def phase4_acceptance(options):
    """Phase-4 coordination acceptance with real Node processes and fixture SDKs.

    A, B and C each run a fixture coordination Agent and a fixture Codex App Server; P is the public
    Node (relay and Gateway); C takes an agent task and is revoked while offline. One browser identity
    uses every entrance.
    """
    binary, module = options.binary.resolve(strict=True), options.crypto_module.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("phase4-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    nodes, clients, roots = [], [], {}
    report = {"status": "running", "cases": {}, "provider_calls": 0, "binary_sha256": file_sha256(binary),
              "coverage": "four real Node processes over iroh (A, B and C each with a fixture coordination Agent and "
                          "fixture App Server, public P); WASM browser client on every entrance",
              "not_checked": ["real provider models", "rendered browser UI (see the browser screenshots)",
                              "a model-written checkpoint summary (phase 4 writes deterministic excerpts)"]}

    def passed(name, **evidence):
        report["cases"][name] = evidence
        print("PASS " + name, flush=True)

    def browser(node):
        client = Client(node.url, helper, identity_file=a.client.identity_file)
        client.network_id = network
        clients.append(client)
        client.connect(node.device_id)
        wait_for(f"the browser is known at {node.name}", lambda: client.authenticate())
        return client

    def records(node, filename="manager-invocations.jsonl"):
        return fixture_records(roots[node.name], filename)

    def turn(node, trigger):
        wait_for(f"{node.name} completes {trigger}", lambda:
                 (scheduler_record(node, network, trigger) or {}).get("state") == "completed", timeout=60)
        return next(row for row in records(node) if row["trigger"] == trigger)

    def works(client):
        return {row["work_id"]: row for row in client.rpc("conversation", conversation_id=conversation)["works"]}

    def delivery(client, work_id):
        rows = client.rpc("conversation", conversation_id=conversation)["deliveries"]
        return next((row["state"] for row in rows if row["work_id"] == work_id), None)

    def work_in(client, work_id, *states):
        row = works(client).get(work_id)
        return row if row and row["state"] in states else None

    def dispatched(node, label):
        return wait_for(f"{node.name} dispatches {label}", lambda: next(
            (row for row in records(node, "manager-native-work.jsonl") if row["label"] == label), None), timeout=60)["work_id"]

    def codex_rpc(node, method):
        return [row for row in records(node, "codex-rpc.jsonl") if row["message"].get("method") == method and not row["bot"]]

    def send(client, message_id, directive, **extra):
        text = directive if isinstance(directive, str) else json.dumps(directive)
        return client.rpc("message", conversation_id=conversation, message_id=message_id, text=text, **extra)

    def folder(name, *, released=False):
        path = workspace / name
        path.mkdir()
        if released:
            (path / "release").touch()
        return path

    def restart(node):
        node.stop()
        node.start()
        wait_for(f"{node.name} aligns after its restart", lambda: node_status(node).get("write_state") == "writable", timeout=60)
        node.client.connect()
        node.client.authenticate()

    try:
        relay_port = available_port()
        relay_url = f"http://127.0.0.1:{relay_port}"
        for name in ("A", "P", "B", "C"):
            nodes.append(Node(run, name, binary, workspace, helper))
        a, p, b, c = nodes
        for node in (a, b, c):
            root = roots[node.name] = run / ("fixture-" + node.name)
            root.mkdir(mode=0o700)
            node.configure(timeout_secs=600, daily_decision_limit=100,
                           bins={"codex": str(fixture_wrapper(root, "fixture-codex", "--fixture-codex"))},
                           app_server=SPAWN_APP_SERVER,
                           manager=fixture_manager_settings(root, workspace))
            node.configure_network(relays=[relay_url])
        p.configure_network(relay_listen=f"127.0.0.1:{relay_port}")
        a.start()
        network = a.bootstrap()
        p.pair(a)
        p.start()
        at_p = browser(p)
        b.pair(a)
        b.start()

        link = at_p.invite()
        log_path = run / "join-C.log"
        with open(log_path, "wb") as log:
            joining = subprocess.Popen([str(binary), "join", "--data-dir", str(c.directory), "--name", "Node C", link],
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
            try:
                listed = wait_for("A lists the join request P holds", lambda: a.client.pending_join(c.device_id))
                require(listed["status"] == "pending", "the request is still pending at its holder")
                a.client.start_approval(listed["join_id"])
                reviewed = wait_for("the joiner's nonce reaches A through P", lambda: (lambda item: item if item and item.get(
                    "joiner_nonce") else None)(a.client.pending_join(c.device_id)))
                code = a.client.command("short_code", request=reviewed)
                wait_for("the CLI shows the same short code", lambda: code.encode() in log_path.read_bytes())
                a.client.grant(reviewed)
                joining.wait(timeout=40)
                require(joining.returncode == 0, "C joins after an approval made at another entrance")
            finally:
                if joining.poll() is None:
                    joining.kill()
                    joining.wait(timeout=5)
        c.start()
        members = (a, p, b, c)
        passed("join-across-entrances", holder="P", approved_at="A")

        conversation = "p4-conversation"
        definition = {"bot_id": "p4-bot", "conversation_id": conversation, "name": "第四期 Bot", "instructions": "",
                      "project": {"target_node": a.device_id, "cwd": str(workspace)}}
        request = {"method": "create_bot", **definition, "scheduler_node": a.device_id,
                   "owner_attestation": a.client.command("attest", network_id=network, scope_id=conversation, body={
                       "type": "conversation_owner", "scheduler_node": a.device_id, "epoch": 1}),
                   "attestation": a.client.attestation("create_bot", dict(definition))}
        at_p.command("drop", method="rpc", params=request)
        at_p.connect(p.device_id)
        at_p.authenticate()
        at_b = browser(b)
        wait_for("P's creation, whose answer was lost, reaches B", lambda: any(
            bot["bot_id"] == "p4-bot" for bot in at_b.rpc("bots")["bots"]), timeout=60)
        retried = at_b.call("rpc", request)["bot"]
        original = next(bot for bot in definitions_of(at_p.rpc("bots")["bots"]) if bot["bot_id"] == "p4-bot")
        require(retried == original, "the same signed creation through another entrance returns the original")
        wait_for("the conversation reaches A", lambda: converged(members, network), timeout=60)
        owners = {client.rpc("conversation", conversation_id=conversation)["conversation"]["owner"] for client in (a.client, at_b, at_p)}
        require(owners == {a.device_id}, "every entrance records the owner the browser signed")
        passed("bot-creation-retry-across-entrances", bot_event=original["event_hash"][:12], owner="A")

        first_dir = folder("p4-first", released=True)
        side_effect = workspace / "p4-side-effect.txt"
        r1 = send(a.client, "p4-m1", {"label": "p4-m1", "host_command": "printf once >> p4-side-effect.txt",
                                      "host_cwd": str(workspace), "dispatch": {"label": "p4-first", "cwd": str(first_dir),
                                                                               "native": {"gate": str(first_dir / "release")}}})
        first = dispatched(a, "p4-first")
        t1 = turn(a, r1["trigger"])
        seen_at_b = wait_for("entrance B shows the work A finished", lambda: work_in(at_b, first, "finished"), timeout=60)
        require(seen_at_b["outcome"] == "completed", "B shows A's result")
        turn(a, "work:" + first)
        r2 = send(at_b, "p4-m2", {"label": "p4-m2", "dispatch": {"label": "p4-continued", "cwd": str(first_dir),
                                                                 "continues": first, "native": {"gate": str(first_dir / "release")}}})
        require(r2["state"] == "pending_delivery", "entrance B keeps the message for owner A")
        continued = dispatched(a, "p4-continued")
        t2 = turn(a, r2["trigger"])
        done = wait_for("the continued work finishes", lambda: work_in(at_b, continued, "finished"), timeout=60)
        rows = works(a.client)
        require(done["thread_id"] == rows[first]["thread_id"], "the follow-up continues the original native thread")
        require(len([row for row in codex_rpc(a, "thread/start") if row["message"]["params"]["cwd"] == str(first_dir)]) == 1,
                "the follow-up starts no second thread")
        require(t2["resumed"] and t2["native_session_id"] == t1["native_session_id"],
                "a message through entrance B continues the owner's native Agent session")
        require("p4-m2" in t2["trigger_text"] and not any("p4-m2" in text for text in t2["messages"]),
                "the turn reads the message sent through B once, as its trigger")
        wait_for("all entrances converge", lambda: converged(members, network), timeout=60)

        def same_views():
            seen = [[(row["message_id"], row["text"]) for row in client.rpc("conversation", conversation_id=conversation)["messages"]]
                    for client in (a.client, at_b, at_p)]
            return seen[0] if seen[0] == seen[1] == seen[2] else None
        shown = wait_for("every entrance shows the same conversation", same_views, timeout=30)
        passed("cross-entrance-context", messages=len(shown), native_session=t1["native_session_id"][:24],
               thread=done["thread_id"][:24], resumed=t2["resumed"])

        task_dir = folder("p4-task")
        task_directive = {"label": "p4-task", "gate": str(task_dir / "task-release"),
                          "dispatch": {"label": "p4-task-child", "cwd": str(task_dir),
                                       "native": {"gate": str(task_dir / "release"), "request": "item/commandExecution/requestApproval",
                                                  "request_mode": "ask"}},
                          "tools": [{"name": "answer_approval", "arguments": {"work_id": "any", "request_id": "any"},
                                     "accepted": False, "rejection": "unknown or unavailable"}]}
        r3 = send(a.client, "p4-m3", {"label": "p4-m3", "dispatch": {"label": "p4-task", "cwd": str(task_dir),
                                                                     "target_node": b.device_id, "native": task_directive}})
        task = dispatched(a, "p4-task")
        turn(a, r3["trigger"])
        child = dispatched(b, "p4-task-child")
        (task_dir / "release").touch()
        question = wait_for("A shows the question B's Agent asked about its App Server request", lambda: next(
            (row for row in a.client.rpc("questions")["questions"] if row["about"]["node"] == b.device_id
             and row["about"].get("request_id")), None), timeout=90)
        question_id = question["question_id"]
        require(question["conversation_id"] == conversation, "B's question lands in the chat that owns the child's thread")
        task_turn = next(row for row in records(b) if row["trigger"] == "task:" + task)
        require(task_turn["kind"] == "agent_task", "B's own Agent runs the task")
        refused = [row for row in records(b, "manager-tool-errors.jsonl") if row["trigger"] == "task:" + task]
        require(len(refused) == 1, "B's Agent has no tool to answer for the user")
        answered = a.client.rpc("answer", question_id=question_id, text="允许", scope_id=conversation)
        require(answered["status"] == "answered" and a.client.rpc("answer", question_id=question_id, text="允许",
                scope_id=conversation)["message"] == answered["message"], "the user's answer at entrance A is written once")
        answer_turn = turn(a, "message:a-" + question_id)
        require(answer_turn["kind"] == "answer", "the answer is a turn of A's bot")
        wait_for("the answered child finishes", lambda: work_in(a.client, child, "finished"), timeout=60)
        answers = [row for row in records(b, "codex-rpc.jsonl") if "method" not in row["message"]
                   and str(row["message"].get("id", "")).startswith("req-")]
        acted = [row for row in records(a, "answer-actions.jsonl") if row["question_id"] == question_id]
        require(len(answers) == 1 and answers[0]["message"]["result"] == {"decision": "accept"} and len(acted) == 1
                and acted[0]["replies"][0] == acted[0]["replies"][1] and acted[0]["replies"][0]["by"] == "agent",
                "A's bot answers B's held request once with respond --node")
        starts = [row for row in codex_rpc(b, "thread/start") if row["message"]["params"]["cwd"] == str(task_dir)]
        require(len(starts) == 1 and starts[0]["message"]["params"].get("approvalPolicy") == "never",
                "B's Agent starts the child itself with full access")
        (task_dir / "task-release").touch()
        finished = wait_for("the task finishes on B", lambda: work_in(a.client, task, "finished"), timeout=60)
        require(finished["outcome"] == "completed" and "p4-task:complete" in finished["result_text"], "the task reports B's Agent result")
        turn(a, "work:" + task)
        wait_for("B's events reach A", lambda: converged(members, network), timeout=60)
        by_b = {event["body"]["type"] for event in scope_events(a, network, conversation) if event["author"] == b.device_id
                and not (event["body"]["type"] == "message_posted" and event["body"]["role"] == "user")}
        require(by_b and by_b <= {"work_dispatched", "work_progress", "work_finished", "question_asked"},
                "B's Agent writes only work events and its question to the conversation")
        passed("agent-task-on-target", task=task, child=child, b_event_types=sorted(by_b), question=question_id,
               answer_turn=answer_turn["trigger"])

        r4 = send(a.client, "p4-m4", {"label": "p4-m4", "tools": [
            {"name": "dispatch_work", "arguments": {"title": "outside", "prompt": "outside", "cwd": str(run), "call_ref": "outside-codex"},
             "accepted": False, "rejection": "outside configured roots"},
            {"name": "dispatch_work", "arguments": {"title": "outside", "prompt": "outside", "cwd": str(run), "call_ref": "outside-agent",
                                                    "target_node": b.device_id}}]})
        turn(a, r4["trigger"])
        remote = next(row for row in records(a, "manager-tools.jsonl") if row["call_ref"] == "outside-agent")["result"]
        require(remote["state"] == "dispatch_rejected" and "outside configured roots" in remote["reason"], "B refuses an agent task outside its roots")
        send(a.client, "p4-unsigned", "no signature", unsigned=True, status=400)
        passed("authority-checks", codex="outside configured roots", agent=remote["reason"][:80], unsigned="rejected")

        revoked_dir = folder("p4-revoked")
        r5 = send(a.client, "p4-m5", {"label": "p4-m5", "dispatch": {"label": "p4-revoked", "cwd": str(revoked_dir),
                                                                     "target_node": c.device_id, "native": {"label": "p4-revoked-task",
                                                                                                            "gate": str(revoked_dir / "release")}}})
        lost = dispatched(a, "p4-revoked")
        turn(a, r5["trigger"])
        wait_for("C's Agent runs the task", lambda: work_in(a.client, lost, "running"), timeout=60)
        c.stop(crash=True)
        stop = a.client.rpc("interrupt_work", conversation_id=conversation, work_id=lost)
        require(stop.get("waiting_for_device") and not stop["confirmed"], "a stop to an offline device waits for it")
        require(delivery(a.client, lost) == "stop_waiting", "the work shows the waiting stop")
        wait_for("the stop request times out", lambda: delivery(a.client, lost) == "stop_timed_out", timeout=60)
        a.client.revoke(c.device_id, head(a, network, c.device_id))
        unknown = wait_for("A leaves the revoked device's work for the user", lambda: (lambda row: row if row["outcome"] == "unverified" else None)(
            works(a.client)[lost]), timeout=60)
        require(unknown["failure_reason"] == "设备已移除，结果无法确认。", "a removed device's work is not written off as never run")
        require(delivery(a.client, lost) is None, "the unknown work drops its stop request")
        passed("offline-stop-and-revoke", stop="stop_timed_out", outcome=unknown["outcome"])

        check_dir = folder("p4-check")
        r6a = send(a.client, "p4-m6a", {"label": "p4-m6a", "dispatch": {"label": "p4-check-done", "cwd": str(check_dir),
                                                                       "native": {"gate": str(check_dir / "release")}}})
        done_id = dispatched(a, "p4-check-done")
        turn(a, r6a["trigger"])
        r6b = send(a.client, "p4-m6b", {"label": "p4-m6b", "dispatch": {"label": "p4-check-not", "cwd": str(check_dir),
                                                                       "native": {"gate": str(check_dir / "release")}}})
        not_id = dispatched(a, "p4-check-not")
        turn(a, r6b["trigger"])
        threads = [wait_for("the check work runs", lambda: work_in(a.client, work_id, "running"))["thread_id"] for work_id in (done_id, not_id)]
        a.stop(crash=True)
        state_path = roots["A"] / "fixture-codex-state.json"
        state = json.loads(state_path.read_text())
        for thread in threads:
            del state["threads"][thread]
        private_json(state_path, state)
        a.start()
        a.client.connect()
        a.client.authenticate()
        turns_before = len(codex_rpc(a, "turn/start"))
        for work_id in (done_id, not_id):
            wait_for("B hears of the unreadable work soon after A restarts", lambda: work_in(at_b, work_id, "unverified"), timeout=20)
        at_b.rpc("resolve", conversation_id=conversation, check_id="work:" + done_id, outcome="completed", note="设备上已看到结果")
        a.client.rpc("resolve", conversation_id=conversation, check_id="work:" + not_id, outcome="interrupted")
        done_row = wait_for("the done choice settles", lambda: work_in(a.client, done_id, "finished"), timeout=60)
        not_row = wait_for("the not-done choice settles", lambda: work_in(a.client, not_id, "finished"), timeout=60)
        require(done_row["outcome"] == "completed" and done_row["result_text"] == "设备上已看到结果"
                and not_row["outcome"] == "interrupted", "each choice becomes the work's outcome")
        settled = [event for event in scope_events(a, network, conversation) if event["body"]["type"] == "work_finished"
                   and event["body"]["work_id"] in (done_id, not_id) and event["body"].get("attestation")]
        require({event["body"]["attestation"]["device_id"] for event in settled} == {a.client.own_id()},
                "both settlements carry the user's signature")
        require({event["author"] for event in settled} == {a.device_id, b.device_id}, "each entrance writes the choice made there")
        require(len(codex_rpc(a, "turn/start")) == turns_before, "settling a check never reruns the work")
        passed("pending-check-choices", recheck="pending", completed=done_id, interrupted=not_id)

        for index in range(6):
            send(a.client, f"p4-pad-{index}", f"p4-pad-{index} " + "背景资料" * 1200, report_results=False)
        for index in range(6):
            turn(a, f"message:p4-pad-{index}")
        for work_id in (done_id, not_id):
            turn(a, "work:" + work_id)
        wait_for("A condenses the long history into a checkpoint", lambda: any(
            event["body"]["type"] == "context_checkpoint" for event in scope_events(a, network, conversation)), timeout=90)
        wait_for("the checkpoint reaches B", lambda: any(
            event["body"]["type"] == "context_checkpoint" for event in scope_events(b, network, conversation)), timeout=60)
        a_calls, b_calls = len(records(a)), len(records(b))
        at_b.rpc("set_owner", conversation_id=conversation, scheduler_node=b.device_id, epoch=2)
        wait_for("every entrance moves the conversation to B", lambda: all(
            client.rpc("conversation", conversation_id=conversation)["conversation"]["owner"] == b.device_id
            for client in (a.client, at_p)), timeout=60)
        r7 = send(a.client, "p4-m7", {"label": "p4-m7"})
        t7 = turn(b, r7["trigger"])
        require(not t7["resumed"] and t7["checkpoint"] and "p4-m7" in t7["trigger_text"],
                "the new owner starts fresh from the checkpoint and the newer messages")
        require_quiet("the switch replays nothing", lambda: side_effect.read_text() == "once"
                      and len(records(a)) == a_calls
                      and [row["trigger"] for row in records(b)[b_calls:]] == [r7["trigger"]])
        passed("owner-switch-without-replay", owner="B", epoch=2, checkpoint=t7["checkpoint"],
               messages_in_prompt=len(t7["messages"]), host_effects=side_effect.read_text())

        payload = run / "attachment.bin"
        with open(payload, "wb") as out:
            for _ in range(64):
                out.write(os.urandom(1024 * 1024))
        size, digest = payload.stat().st_size, file_sha256(payload)
        offset, restarted_at, sent_after = 0, None, 0
        with open(payload, "rb") as source:
            while True:
                source.seek(offset)
                chunk = source.read(1024 * 1024)
                reply = a.client.rpc("upload_attachment", conversation_id=conversation, hash=digest, size=size,
                                     offset=offset, data=base64.b64encode(chunk).decode())
                if restarted_at is not None:
                    sent_after += len(chunk)
                if reply["complete"]:
                    break
                offset = reply["offset"]
                if restarted_at is None and offset >= 24 * 1024 * 1024:
                    restart(a)
                    restarted_at, offset = offset, 0
        require(restarted_at and sent_after <= size - restarted_at + 1024 * 1024, "the upload resumes after a restart")
        b.stop()
        receipt = send(a.client, "p4-attachment", "请看附件", attachments=[{"hash": digest, "size": size, "name": "p4 附件.bin"}], report_results=False)
        partial = b.directory / "blobs" / "partial" / (digest + ".part")
        b.start()
        wait_for("B starts downloading", lambda: partial.exists() and partial.stat().st_size >= 4 * 1024 * 1024
                 and committed_blob_offset(b, network, digest) > 0, timeout=60)
        b.stop(crash=True)
        kept = committed_blob_offset(b, network, digest)
        b.start()
        stored = b.directory / "blobs" / "sha256" / digest[:2] / digest
        wait_for("B finishes the attachment", lambda: stored.exists() and stored.stat().st_size == size, timeout=120)
        require(file_sha256(stored) == digest and 0 < kept < size, "B resumes the download and verifies its hash")
        at_b.connect(b.device_id)
        wait_for("the browser reconnects to the restarted B", lambda: at_b.authenticate())
        shown = next(row for row in at_b.rpc("conversation", conversation_id=conversation)["messages"] if row["message_id"] == "p4-attachment")
        require(shown["attachments"] == [{"hash": digest, "size": size, "name": "p4 附件.bin"}],
                "the message carries its named attachment on every entrance")
        download, offset, pages = hashlib.sha256(), 0, 0
        while offset is not None:
            page = at_b.rpc("read_artifact", conversation_id=conversation, blob_hash=digest, offset=offset, binary=True)
            download.update(base64.b64decode(page["data"]))
            offset, pages = page["next_offset"], pages + 1
        require(download.hexdigest() == digest, "the new entrance downloads the attachment byte for byte")
        payload.unlink()
        passed("attachment-64mib-restart-resume", size=size, upload_restart_at=restarted_at, download_kept=kept,
               message=receipt["message_id"], binary_pages_at_b=pages)

        require(file_sha256(binary) == report["binary_sha256"], "one immutable binary completes acceptance")
        report["status"] = "passed"
    except BaseException as error:
        report.update(status="failed", failure=f"{type(error).__name__}: {error}", traceback=traceback.format_exc())
        raise
    finally:
        for client in clients:
            client.close()
        for node in reversed(nodes):
            node.client.close()
            try:
                node.stop()
            except subprocess.TimeoutExpired:
                report.setdefault("slow_stop", []).append(node.name)
        for root in roots.values():
            stop_fixture_processes(root)
        report["cleanup"] = {"remaining_nodes": sum(node.process is not None and node.process.poll() is None for node in nodes)}
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


def public_entry_acceptance(options):
    """A public entry P that is no member: web, relay and forwarding only. M is the only member Node and
    is reachable only through P's relay; a browser opens P's origin, joins M and chats with it."""
    binary, module = options.binary.resolve(strict=True), options.crypto_module.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("public-entry-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    nodes, clients = [], []
    report = {"status": "running", "cases": {}, "provider_calls": 0, "binary_sha256": file_sha256(binary),
              "coverage": "public entry P (not a member) with its relay; member M on IPv6-only transport; "
                          "Node X holding a wrong relay token; WASM browser client on P's origin",
              "not_checked": ["real NAT and TLS reverse proxy (loopback ports stand in for nginx paths)",
                              "rendered browser UI (see the browser screenshots)"]}

    def passed(name, **evidence):
        report["cases"][name] = evidence
        print("PASS " + name, flush=True)

    def entry_rows(path):
        with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
            return {table: db.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
                    for table in ("networks", "events", "conversations", "log_heads")}

    try:
        relay_port = available_port()
        relay_url = f"http://127.0.0.1:{relay_port}"
        p = Node(run, "P", binary, workspace, helper,
                 init_args=["--public-entry", "--relay-listen", f"127.0.0.1:{relay_port}", "--relay-url", relay_url])
        nodes.append(p)
        token = p.directory / "entry-token"
        require(token.stat().st_mode & 0o077 == 0 and len(token.read_text().strip()) == 64,
                "the public entry keeps a private operator token")
        require(not (p.directory / "bootstrap.json").exists(), "a public entry has no network setup capability")
        wrong = run / "wrong-token"
        wrong.write_text(uuid.uuid4().hex + uuid.uuid4().hex)
        wrong.chmod(0o600)
        fixture = run / "fixture-M"
        fixture.mkdir(mode=0o700)
        m = Node(run, "M", binary, workspace, helper, init_args=[
            "--root", str(workspace), "--entry", p.url, "--entry-relay", relay_url, "--relay-token-file", str(token)])
        x = Node(run, "X", binary, workspace, helper, init_args=[
            "--root", str(workspace), "--entry", p.url, "--entry-relay", relay_url, "--relay-token-file", str(wrong)])
        nodes += [m, x]
        for node in (m, x):
            node.configure_network(bind=f"[::1]:{node.port}")
        m.configure(timeout_secs=600, daily_decision_limit=100, manager=fixture_manager_settings(fixture, workspace),
                    bins={"codex": str(fixture_wrapper(fixture, "fixture-codex", "--fixture-codex"))},
                    app_server=SPAWN_APP_SERVER)
        for node in nodes:
            node.start()
        info = p.client.request("/api/info")
        require(info["public_entry"] is True, "the entry says it is a public entry")
        p.client.request("/api/local/setup", {}, status=404)
        passed("public-entry-starts-without-a-network", device=p.device_id, relay=relay_url)

        network = m.bootstrap()
        x.bootstrap()
        m.client.call("subscribe")
        listed = wait_for("M advertises the public entry as its Gateway", lambda: next(
            (row for row in m.client.call("entrances")["entrances"]
             if json.loads(row["payload"])["device_id"] == m.device_id
             and json.loads(row["payload"]).get("gateway_url") == p.url), None), timeout=60)
        require(listed, "M's signed address names P")
        passed("member-names-the-public-entry", gateway_url=p.url)

        phone = Client(p.url, helper, identity_file=run / "phone-identity.json")
        clients.append(phone)
        started = time.monotonic()
        phone.join(m.client, "phone", via=p.url)
        require(phone.url == p.url, "the phone joined through P's origin")
        passed("browser-joins-the-member-through-the-public-entry", elapsed_ms=round((time.monotonic() - started) * 1000))

        conversation = "public-entry-chat"
        phone.rpc("create_conversation", conversation_id=conversation, title="经公共入口")
        phone.rpc("message", conversation_id=conversation, message_id="pe-1", text="你好")
        wait_for("M answers the phone", lambda: any(row["role"] == "assistant" for row in
                 phone.rpc("conversation", conversation_id=conversation)["messages"]), timeout=60)
        passed("browser-chats-through-the-public-entry")

        phone.close()
        p.stop()
        p.start()
        wait_for("the phone reconnects through the restarted entry", lambda: phone.connect(m.device_id, check=False).get("ok"), timeout=60)
        phone.authenticate(network)
        phone.rpc("message", conversation_id=conversation, message_id="pe-2", text="又来了")
        wait_for("M answers after the reconnect", lambda: sum(row["role"] == "assistant" for row in
                 phone.rpc("conversation", conversation_id=conversation)["messages"]) >= 2, timeout=60)
        passed("browser-reconnects-after-the-entry-restarts")

        refused = Client(p.url, helper, identity_file=run / "outsider-identity.json")
        clients.append(refused)
        outcome = refused.connect(x.device_id, check=False)
        require(not outcome.get("ok"), "P cannot reach a Node whose relay token is wrong")
        direct = Client(x.url, helper, identity_file=run / "outsider-identity.json")
        clients.append(direct)
        require(direct.connect(x.device_id, check=False).get("ok"), "X itself is running")
        passed("relay-refuses-a-wrong-token", error=str(outcome.get("error", ""))[:200])

        work_dir = workspace / "pe-work"
        work_dir.mkdir()
        (work_dir / "release").touch()
        phone.rpc("message", conversation_id=conversation, message_id="pe-3", text=json.dumps({"label": "pe-3", "dispatch": {
            "label": "pe-work", "cwd": str(work_dir), "native": {"gate": str(work_dir / "release")}}}))
        thread_id = wait_for("M's Agent starts a Codex thread", lambda: next((thread_id for thread_id, thread in json.loads(
            (fixture / "fixture-codex-state.json").read_text())["threads"].items() if thread["cwd"] == str(work_dir)), None)
            if (fixture / "fixture-codex-state.json").exists() else None, timeout=60)
        local_post(m, "/api/hooks", {"source": "hook", "agent": "codex", "event": "turn_end", "session_id": thread_id,
                                     "turn_id": "pe-hook", "cwd": str(work_dir), "last_message": "需要你决定 needs-human"})
        question = wait_for("M's Agent asks in the chat that owns the thread", lambda: next(
            (row for row in m.client.rpc("questions")["questions"] if row["about"]["session_id"] == thread_id), None), timeout=90)
        question_id = question["question_id"]
        token = m.client.call("create_decision_token", {"label": "grok"})["token"]
        decision = {"device_id": m.device_id, "question_id": question_id, "text": "经入口送达的回答"}
        sent = p.client.request("/api/decision", decision, origin=False, headers={"Authorization": "Bearer " + token}, timeout=60)
        again = p.client.request("/api/decision", decision, origin=False, headers={"Authorization": "Bearer " + token}, timeout=60)
        p.client.request("/api/decision", decision, origin=False,
                         headers={"Authorization": "Bearer vfd_" + "0" * 64}, status=401, timeout=60)
        acted = wait_for("M's bot passes the answer on", lambda: [row for row in fixture_records(fixture, "answer-actions.jsonl")
                         if row["question_id"] == question_id] or None, timeout=90)
        answer_work = acted[0]["replies"][0]["work_id"]
        sends = [row for row in fixture_records(fixture, "codex-rpc.jsonl") if row["message"].get("method") in ("turn/start", "turn/steer")
                 and row["message"]["params"].get("clientUserMessageId") == answer_work
                 and acted[0]["replies"][1]["work_id"] == answer_work]
        require(sent == again == {"question_id": question_id, "answer_id": "a-" + question_id, "status": "answered",
                                  "text": "经入口送达的回答"} and question["conversation_id"] == conversation
                and len(acted) == 1 and len(sends) == 1,
                "a decision token's POST through P answers M's question once and the answer reaches the thread once; a wrong token gets 401")
        require(token not in (p.directory / "process.log").read_text(errors="replace"), "P does not log the token")
        passed("decision-through-the-public-entry", question=question_id, thread=thread_id[:24])

        rows = entry_rows(p.directory / "node.sqlite3")
        require(all(count == 0 for count in rows.values()), "P stores no events, conversations or logs")
        require(not any((p.directory / "blobs").rglob("*")) if (p.directory / "blobs").exists() else True,
                "P stores no attachments")
        passed("public-entry-stores-nothing", **rows)
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
        stop_fixture_processes(run / "fixture-M") if (run / "fixture-M").exists() else None
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


def discovery_acceptance(options):
    binary, module = options.binary.resolve(strict=True), options.crypto_module.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("discovery-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    nodes = []
    report = {"status": "running", "cases": {}, "provider_calls": 0, "binary_sha256": file_sha256(binary),
              "not_checked": ["public NAT types and UDP-blocked networks", "rendered browser UI", "live Agent tool execution"]}

    def passed(name, **evidence):
        report["cases"][name] = evidence
        print("PASS " + name, flush=True)

    def clear_addresses(*members):
        for node in members:
            require(node.process is None, "only stopped task-owned Nodes have their cached paths cleared")
            with sqlite3.connect(node.directory / "node.sqlite3") as database:
                database.execute("DELETE FROM addresses")

    def peer(node, target):
        return next((row for row in node_status(node).get("peers", []) if row["device_id"] == target.device_id), {})

    def reconnect(node, network):
        node.client.close()
        node.client.connect(node.device_id)
        node.client.authenticate(network)
        wait_for("the management Node is aligned and writable", lambda: node_status(node).get("write_state") == "writable", timeout=60)

    def stopped_browser(node):
        try:
            with socket.create_connection(("127.0.0.1", node.port), timeout=1):
                raise AssertionError("a headless Node still has a public TCP listener")
        except (ConnectionRefusedError, TimeoutError):
            pass
        local = Client(f"http://127.0.0.1:{node.local_port}", None)
        for path in ("/", "/api/info", "/api/client", "/api/local/info", "/generated/vibeface_secure_bg.wasm"):
            local.request(path, status=404)
        hook = json.loads(node_cli(node, "hook", "codex", "--test").stdout)
        require(hook["status"] == "accepted", "the headless Node handles a real local hook")
        tool_port = int((node.directory / "tool-port").read_text())
        tools = Client(f"http://127.0.0.1:{tool_port}", None)
        tools.request("/sdk.v1.SdkCustomToolCallbackService/CallCustomTool",
                      {"toolName": "info", "agentId": "not-an-agent", "args": {}},
                      status=401, origin=False, headers={"Authorization": "Bearer invalid-acceptance-token"})
        return node_status(node)

    try:
        discovery_port = available_port()
        d = Node(run, "D", binary, workspace, helper, init_args=[
            "--public-entry", "--relay-mode", "discovery-only", "--relay-listen", f"0.0.0.0:{discovery_port}"])
        nodes.append(d)
        require(not (d.directory / "entry-token").exists(), "discovery has no data-relay operator token")
        d.start()
        require(b"<html" in d.client.request("/").lower(), "discovery defaults to serving its embedded webpage")
        for path in ("/api/client", "/api/decision", "/api/local/setup"):
            d.client.request(path, status=404)
        try:
            with socket.create_connection(("127.0.0.1", discovery_port), timeout=1):
                raise AssertionError("discovery must not run the TCP business relay")
        except ConnectionRefusedError:
            pass
        passed("discovery-has-no-business-relay-or-http-proxy")
        address = f"vibeface://{d.device_id}@localhost:{discovery_port}"
        a, b = [Node(run, name, binary, workspace, helper) for name in ("A", "B")]
        nodes += [a, b]
        for node in (a, b):
            node.configure_network(discovery=[address])
        a.start()
        network = a.bootstrap()
        b.pair(a, discovery_only=True)
        b.start()
        wait_for("discovery establishes A-to-B direct sync", lambda: peer(a, b).get("path") == "direct", timeout=60)
        a.client.rpc("create_conversation", conversation_id="discovery-first", title="Discovery direct sync")
        wait_for("discovery members sync their first event", lambda: converged((a, b), network), timeout=60)
        passed("cli-join-with-only-target-id-and-invite-uses-discovery", path=peer(a, b)["path"])

        for node in (a, b):
            node.client.close()
            node.stop()
        clear_addresses(a, b)
        d.stop()
        d.start(no_web=True)
        a.start()
        b.start()
        wait_for("fresh caches recover through restarted discovery", lambda: peer(a, b).get("path") == "direct", timeout=60)
        reconnect(a, network)
        a.client.rpc("create_conversation", conversation_id="discovery-restart", title="Discovery restart")
        wait_for("restarted discovery restores synchronization", lambda: converged((a, b), network), timeout=60)
        passed("discovery-restart-restores-direct-sync-with-empty-address-caches", path=peer(a, b)["path"], web_enabled=node_status(d)["web_enabled"])

        for node in (a, b):
            node.client.close()
            node.stop()
        b.configure_network(bind=f"[::1]:{b.port}")
        clear_addresses(a, b)
        a.start()
        b.start()
        wait_for("disjoint IP families cannot complete business through discovery", lambda: peer(a, b).get("reachable") is False, timeout=45)
        reconnect(a, network)
        a.client.rpc("create_conversation", conversation_id="discovery-blocked", title="Direct path blocked")
        require_quiet("discovery never forwards the blocked event", lambda: heads(b, network) != heads(a, network))
        require(peer(a, b).get("path") is None, "no data path is hidden behind discovery")
        passed("blocked-direct-path-does-not-fall-back-to-discovery")

        b.stop()
        b.configure_network(bind=f"127.0.0.1:{b.port}")
        b.start()
        wait_for("restored IP path catches up", lambda: converged((a, b), network), timeout=60)
        relay_port = available_port()
        relay_url = f"http://127.0.0.1:{relay_port}"
        r = Node(run, "R", binary, workspace, helper, init_args=[
            "--relay-listen", f"127.0.0.1:{relay_port}", "--relay-url", relay_url])
        nodes.append(r)
        r.pair(a)
        r.start()
        for node in (a, b):
            node.client.close()
            node.stop()
            node.configure_network(relays=[relay_url])
            node.start()
        reconnect(a, network)
        wait_for("members publish normal relay paths", lambda: all(
            any("Relay" in item for item in json.loads(row["payload"])["endpoint"]["addrs"])
            for row in a.client.call("entrances")["entrances"]), timeout=60)
        wait_for("relay members align before isolating B", lambda: converged((a, b, r), network), timeout=60)
        for node in (a, b):
            node.client.close()
            node.stop()
        b.configure_network(bind=f"[::1]:{b.port}")
        a.start()
        b.start()
        wait_for("ordinary relay recovers the disjoint direct paths automatically", lambda: peer(a, b).get("path") == "relay", timeout=60)
        reconnect(a, network)
        a.client.rpc("create_conversation", conversation_id="discovery-relay", title="Automatic relay fallback")
        wait_for("business sync succeeds on the normal relay", lambda: converged((a, b, r), network), timeout=60)
        passed("normal-relay-fallback-is-automatic", path=peer(a, b)["path"])

        b.stop()
        b.start(no_web=True)
        status = stopped_browser(b)
        a.client.rpc("create_conversation", conversation_id="discovery-headless", title="Headless sync")
        wait_for("headless B still synchronizes", lambda: converged((a, b, r), network), timeout=60)
        wait_for("B withdraws its closed webpage", lambda: any(
            json.loads(row["payload"])["device_id"] == b.device_id and json.loads(row["payload"])["gateway_url"] is None
            for row in a.client.call("entrances")["entrances"]), timeout=60)
        passed("serve-no-web-keeps-hooks-tool-auth-maintenance-and-sync", web_enabled=status["web_enabled"])
        b.stop()
        b.start(no_web=True, bare=True)
        stopped_browser(b)
        b.stop()
        b.configure(web_enabled=False)
        b.start()
        stopped_browser(b)
        passed("bare-no-web-and-persisted-web-off")
        with sqlite3.connect(f"file:{d.directory / 'node.sqlite3'}?mode=ro", uri=True) as database:
            counts = {table: database.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
                      for table in ("networks", "events", "conversations", "log_heads")}
        require(all(count == 0 for count in counts.values()), "discovery stores no personal network or business records")
        passed("discovery-stores-no-business-records", **counts)
        report["status"] = "passed"
    except BaseException as error:
        report.update(status="failed", failure=f"{type(error).__name__}: {error}", traceback=traceback.format_exc())
        raise
    finally:
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


def node_status(node):
    result = subprocess.run([str(node.binary), "status", "--json", "--data-dir", str(node.directory)],
                            capture_output=True, timeout=20)
    return json.loads(result.stdout)


def phase5_acceptance(options):
    """Write protection, logs and troubleshooting with real Node processes.

    A and B are member Nodes (B runs the relay). A is restored from a backup, restarted alone, and its
    data directory is copied and run as A' while A is stopped. One browser identity opens A and B.
    """
    binary, module = options.binary.resolve(strict=True), options.crypto_module.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("phase5-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    nodes, clients = [], []
    report = {"status": "running", "cases": {}, "provider_calls": 0, "binary_sha256": file_sha256(binary),
              "coverage": "two real Node processes over iroh plus a copy of A's data directory; WASM browser client",
              "not_checked": ["rendered browser UI (see the browser screenshots)", "no App Server runs in this mode"]}
    created = iter(range(1, 1000))

    def passed(name, **evidence):
        report["cases"][name] = evidence
        print("PASS " + name, flush=True)

    def write(client, *, status=200):
        name = f"p5-{next(created)}"
        return client.rpc("create_conversation", conversation_id=name, title=name, status=status)

    def state(client):
        return client.call("write_state")

    def reopen(node):
        node.start()
        node.client.connect()
        node.client.authenticate()

    def at_b():
        browser.connect(b.device_id)
        browser.authenticate()
        return browser

    def until(node, wanted, timeout=90):
        return wait_for(f"{node.name} writes {wanted}", lambda: state(node.client)["state"] == wanted or None, timeout=timeout)

    try:
        relay_port = available_port()
        relay_url = f"http://127.0.0.1:{relay_port}"
        a = Node(run, "A", binary, workspace, helper)
        b = Node(run, "B", binary, workspace, helper)
        nodes += [a, b]
        for node in nodes:
            node.configure(app_server={"mode": "system", "socket": str(run / "unavailable.sock")})
        b.configure_network(relay_listen=f"127.0.0.1:{relay_port}")
        a.configure_network(relays=[relay_url])
        a.start()
        network = a.bootstrap()
        b.pair(a)
        b.start()
        browser = Client(b.url, helper, identity_file=a.client.identity_file)
        browser.network_id = network
        clients.append(browser)
        wait_for("the browser is known at B", at_b)
        write(a.client)
        wait_for("A and B converge", lambda: converged(nodes, network), timeout=60)
        shown = wait_for("A's status shows B synced", lambda: (lambda s: s if s["peers"] and s["peers"][0]["synced_ms"] else None)(node_status(a)))
        require(shown["write_state"] == "writable" and shown["peers"][0]["device_id"] == b.device_id
                and shown["peers"][0]["reachable"] is True, "status reports writes and the reachable peer")
        text = subprocess.run([str(binary), "status", "--data-dir", str(a.directory)], capture_output=True, timeout=20).stdout.decode()
        require("Writes: writable" in text and re.search(r"Peer: .* reachable, synced \d+s ago", text), "plain status names the peer and its last sync")
        passed("status-shows-writes-and-peer-sync", peers=len(shown["peers"]))

        a.stop()
        backup = run / "A-backup"
        shutil.copytree(a.directory, backup)
        reopen(a)
        until(a, "writable")
        write(a.client)
        write(a.client)
        ahead = wait_for("B holds A's newer events", lambda: converged(nodes, network), timeout=60)[a.device_id]
        a.stop()
        b.stop()
        shutil.rmtree(a.directory)
        shutil.copytree(backup, a.directory)
        reopen(a)
        before = state(a.client)["state"]
        require(before in ("aligning", "read_only"), "a restored Node does not start writable")
        refused = write(a.client, status=400)["error"]
        require(re.search(r"aligning|read-only", refused), "a restored Node refuses writes before it aligns")
        b.start()
        until(a, "writable")
        require(heads(a, network)[a.device_id] >= ahead, "A took back the events it wrote after the backup")
        write(a.client)
        wait_for("A and B converge after the restore", lambda: converged(nodes, network), timeout=60)
        require(not state(a.client)["conflicts"] and not state(at_b())["conflicts"], "no fork after the restore")
        passed("restored-backup-refuses-writes-until-aligned", before=before, refused=refused[:120], own_seq_from_peer=ahead)

        b.stop()
        a.stop()
        reopen(a)
        require(state(a.client)["state"] == "writable", "restarted from the file it last wrote from, A writes alone")
        write(a.client)
        alone = node_status(a)
        require(alone["write_state"] == "writable" and alone["peers"][0]["reachable"] is not True, "status shows writes and the absent peer")
        passed("restart-from-the-same-data-writes-while-peers-are-offline")

        a.stop()
        copied = run / "A-copy"
        shutil.copytree(a.directory, copied)
        b.start()
        reopen(a)
        until(a, "writable")
        original = f"p5-{next(created)}"
        a.client.rpc("create_conversation", conversation_id=original, title=original)
        wait_for("B holds A's newest event", lambda: converged(nodes, network), timeout=60)
        a.stop()
        b.stop()
        twin = copy.copy(a)
        twin.name, twin.directory, twin.process, twin.log = "A-copy", copied, None, None
        nodes.append(twin)
        reopen(twin)
        until(twin, "read_only")
        refused = write(twin.client, status=400)["error"]
        require("read-only" in refused, "alone, the copy refuses writes until the user confirms")
        require(twin.client.call("confirm_writable")["state"] == "writable", "the user's confirmation makes the copy writable")
        write(twin.client)
        b.start()
        until(twin, "forked")
        refused = write(twin.client, status=400)["error"]
        require("forked" in refused, "the copy stops writing")
        kept = state(twin.client)["conflicts"]
        require(kept, "the copy lists the events kept aside")
        require(node_status(twin)["write_state"] == "forked", "status shows the fork")
        require(b"writes stopped" in (copied / "node.log").read_bytes(), "the copy's log says why it stopped")
        diagnostics = twin.client.call("diagnostics")
        require(diagnostics["write_state"] == "forked" and diagnostics["conflicts"] >= 1
                and diagnostics["logs"].get("node.log"), "the diagnostics export carries the fork and the log tail")
        private_json(run / "diagnostics.json", diagnostics)
        viewer = at_b()
        wait_for("B holds both copies' events", lambda: state(viewer)["conflicts"] or None, timeout=60)
        twin.stop()
        reopen(a)
        until(a, "forked")
        require("forked" in write(a.client, status=400)["error"], "the original stops too once it sees the copy's events")
        listed = lambda: {row["conversation_id"] for row in viewer.rpc("conversations")["conversations"]}
        require(original not in listed(), "neither copy's events count from the fork on")
        write(viewer)
        viewer.revoke(a.device_id, head(b, network, a.device_id))
        wait_for("removing A counts the branch B holds", lambda: original in listed() or None, timeout=30)
        passed("copied-data-directory-forks-and-removal-keeps-one-branch", conflicts=len(kept), refused=refused[:120])

        secrets = [node.access["capability"] for node in (a, b)] + [a.backup_password]
        logs = [path for path in run.rglob("*.log*") if path.is_file()]
        require(all(path.stat().st_mode & 0o077 == 0 for path in logs if path.name.startswith("node.log")), "node.log is private")
        require(all(b"write state changed" in (node.directory / "node.log").read_bytes() for node in (a, b)),
                "a foreground Node writes info lines to node.log")
        exported = json.dumps(diagnostics)
        leaked = [str(path) for path in logs if any(secret.encode() in path.read_bytes() for secret in secrets)
                  or b"vibeface1" in path.read_bytes()]
        require(not leaked, f"no capability, passphrase or invite in logs: {leaked}")
        require(not any(secret in exported for secret in secrets) and "vibeface1" not in exported,
                "the diagnostics export holds no capability, passphrase or invite")
        epoch = json.loads((a.directory / "data-epoch.json").read_text())["data_epoch"]
        require(epoch == DATA_EPOCH, "data epoch matches the build")
        passed("logs-and-diagnostics-hold-no-secrets", logs=len(logs), data_epoch=epoch)
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
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


def upgrade_status(node, path, origin):
    """The status of a WebSocket upgrade to `path` sent as a page on `origin` would send it."""
    connection = http.client.HTTPConnection("127.0.0.1", node.port, timeout=5)
    headers = {"Host": f"127.0.0.1:{node.port}", "Upgrade": "websocket", "Connection": "Upgrade",
               "Sec-WebSocket-Key": base64.b64encode(os.urandom(16)).decode(), "Sec-WebSocket-Version": "13"}
    if origin:
        headers["Origin"] = origin
    try:
        connection.request("GET", path, headers=headers)
        return connection.getresponse().status
    finally:
        connection.close()


def auto_route_acceptance(options):
    """Entry acceptance: ticket joins, revocation, origins, unknown writes, recovery and, once a second
    Node has joined, forwarding through its Gateway to the target over iroh."""
    repository = Path(__file__).resolve().parents[1]
    binary, module = options.binary.resolve(strict=True), options.crypto_module.resolve(strict=True)
    artifacts = options.artifacts.resolve()
    require(artifacts.is_relative_to(repository / ".artifacts"), "entry fixtures stay in the task worktree")
    artifacts.mkdir(parents=True, exist_ok=True)
    run = artifacts / ("auto-route-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir()
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    nodes, clients = [], []
    allowed = "https://client.vibeface.example"
    report = {"status": "running", "source_commit": subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repository, text=True).strip(),
        "binary_sha256": file_sha256(binary), "provider_calls": 0, "cases": {}, "artifacts": str(run),
        "not_checked": ["rendered browser UI", "production ingress", "provider models"]}

    def fresh(url):
        client = Client(url, helper)
        clients.append(client)
        return client

    def passed(name, **evidence):
        report["cases"][name] = evidence
        print("PASS auto-route " + name, flush=True)

    try:
        target = Node(run, "TARGET", binary, workspace, helper)
        nodes.append(target)
        target.configure(manager=fixture_manager_settings(run, workspace), bins={"codex": str(fixture_wrapper(run, "fixture-codex", "--fixture-codex"))},
                         app_server=SPAWN_APP_SERVER, daily_decision_limit=100, allowed_origins=[allowed])
        target.start()
        network = target.bootstrap()
        passed("genesis", network=network, target=target.device_id)

        def local_entry():
            return json.loads(node_cli(target, "access", "--json", "--data-dir", str(target.directory)).stdout)
        require(local_entry().get("local") is True, "a new network opens local browser access without confirmation")
        node_cli(target, "access", "--disable", "--data-dir", str(target.directory))
        require(not json.loads((target.directory / "config.json").read_text())["local_auto_access"]
                and local_entry().get("local") is None, "access --disable turns it off in the running Node")
        node_cli(target, "access", "--enable", "--data-dir", str(target.directory))
        require(local_entry().get("local") is True, "access --enable turns it back on without a restart")
        passed("access-cli-toggle", node=target.device_id)

        impostor = fresh(target.url)
        other_id = impostor.own_id()
        refused = impostor.connect(other_id, check=False)
        require(not refused["ok"], "a Gateway refuses a target that is not itself")
        passed("wrong-target-refused", error=refused["error"])

        require(upgrade_status(target, "/api/client", allowed) == 101, "allowed_origins may open /api/client")
        require(upgrade_status(target, "/api/client", "https://elsewhere.example") == 403,
                "other origins cannot open /api/client")
        require(upgrade_status(target, "/api/local/ws", allowed) == 403, "other paths stay same-origin")
        target.client.request("/api/info", headers={"Origin": allowed}, origin=False, status=403)
        cross = fresh(target.url)
        cross.connect(target.device_id, origin=allowed)
        require(cross.call("info")["device_id"] == target.device_id, "a cross-origin page completes Noise to the target")
        passed("client-origins", allowed=allowed)

        outsider = fresh(target.url)
        outsider.connect(target.device_id)
        require(outsider.call("authenticate")["principal"] is None, "an unknown device has no principal")
        outsider.call("rpc", {"method": "nodes"}, status=401)
        passed("outsider-unauthorized")

        manager = fresh(target.url)
        manager.join(target, "Second manager", manage=True)
        require(manager.principal["can_manage"], "a manager can grant management at approval")
        member = fresh(target.url)
        member.join(manager, "Plain member")
        require(not member.principal["can_manage"], "a plain grant does not manage")
        member.call("invite", {"record": member.command("invite", network_id=network, entry_id=target.device_id)["record"]},
                    status=403)
        passed("join-and-grant-manage", manager=manager.own_id(), member=member.own_id())

        mismatch = fresh(target.url)
        voided = mismatch.join(target, "Mismatched", reveal="ab" * 32)
        require(voided["status"] == "rejected" and voided["reason"] == "commitment_mismatch",
                "a reveal that differs from the commitment voids the request")
        again = mismatch.call("join_status", {"join_id": voided["join_id"]})
        require(again["status"] == "rejected", "a voided request stays void")
        passed("commitment-mismatch-voids", join_id=voided["join_id"])

        rejected = fresh(target.url)
        link = target.client.invite()
        ticket = rejected.command("ticket", ticket=link.split("#", 1)[1])
        rejected.connect(ticket["entry_id"])
        nonce = uuid.uuid4().hex * 2
        request = rejected.call("join", {"secret": ticket["secret"], "public_key": rejected.own_id(), "kind": "client",
                                         "name": "Rejected", "commit": rejected.command("commit", nonce=nonce)})
        target.client.start_approval(request["join_id"])
        rejected.call("join_status", {"join_id": request["join_id"], "reveal": nonce})
        reviewed = target.client.pending_join(rejected.own_id())
        target.client.call("approve", {"join_id": request["join_id"], "reject": True})
        require(rejected.call("join_status", {"join_id": request["join_id"]})["status"] == "rejected",
                "rejecting the short code voids the request")
        member_value = {"device_id": reviewed["device_id"], "kind": "client", "name": "Rejected", "manage": False}
        late = target.client.command("governance", state=target.client.directory_state(), network_id=network,
                                     action={"type": "grant", "member": member_value})
        target.client.call("approve", {"join_id": request["join_id"], "event": late}, status=400)
        passed("rejection-voids", join_id=request["join_id"])

        target.client.revoke(member.own_id())
        member.call("rpc", {"method": "nodes"}, status=401)
        member.connect()
        auth = member.call("authenticate")
        require(auth["principal"] is None and auth["revoked"], "a revoked browser is told it was removed")
        passed("revocation", revoked=member.own_id())

        conversation = "entry-conversation"
        target.client.rpc("create_conversation", conversation_id=conversation, title="Entry acceptance")
        unsigned = {"conversation_id": conversation, "message_id": "unsigned", "text": "no signature"}
        target.client.rpc("message", unsigned=True, status=400, **unsigned)
        forged = {**unsigned, "attestation": manager.attestation("message", dict(unsigned))}
        target.client.call("rpc", {"method": "message", **forged}, status=400)
        changed = {**unsigned, "attestation": target.client.attestation("message", dict(unsigned)), "text": "changed"}
        target.client.call("rpc", {"method": "message", **changed}, status=400)
        passed("attestation-required", refused=["unsigned", "other-device", "changed-text"])

        directive = json.dumps({"label": "auto-route", "host_command": "printf once >> auto-route-count.txt",
                                "host_cwd": str(workspace), "gate": str(run / "never-release")})
        original = {"conversation_id": conversation, "message_id": "auto-route-unknown", "text": directive,
                    "report_results": False}
        signed = {**original, "attestation": target.client.attestation("message", dict(original))}
        target.client.command("drop", method="rpc", params={"method": "message", **signed})
        marker = workspace / "auto-route-count.txt"
        wait_for("exactly one Agent write starts after the dropped send", lambda:
                 marker.exists() and marker.read_text() == "once" and len(invocations(run, "auto-route")) == 1)
        target.client.connect()
        target.client.authenticate()
        snapshot = target.client.rpc("conversation", conversation_id=conversation)
        require(any(row["message_id"] == original["message_id"] for row in snapshot["messages"]),
                "after reconnecting, the business ID shows the write was recorded")
        trigger = invocations(run, "auto-route")[0]["trigger"]
        duplicate = target.client.call("rpc", {"method": "message", **signed})
        require(duplicate["trigger"] == trigger, "resubmitting the unchanged signed request returns the original job")
        require_quiet("a same-ID resubmission never repeats the host write", lambda:
                      marker.read_text() == "once" and len(invocations(run, "auto-route")) == 1)
        passed("unknown-write-not-replayed", message_id=original["message_id"], trigger=trigger, invocations=1)

        joining = Node(run, "CLI", binary, workspace, helper)
        nodes.append(joining)
        code = joining.pair(target)
        listed = target.client.call("directory")["view"]["members"]
        require(any(row["device_id"] == joining.device_id and row["kind"] == "node" for row in listed),
                "the CLI Node is a member after the grant")
        passed("cli-join", device=joining.device_id, short_code_digits=len(code))

        joining.start()
        through = fresh(joining.url)
        through.join(target, "Browser on the second Gateway")
        through.url = joining.url
        through.connect(target.device_id)
        wait_for("the second Gateway forwards to the target", lambda: through.authenticate())
        require(through.call("info")["device_id"] == target.device_id, "Noise ends at the target behind the other Gateway")
        require(any(row["message_id"] == original["message_id"]
                    for row in through.rpc("conversation", conversation_id=conversation)["messages"]),
                "the forwarded channel reads the target's conversation")
        stranger = fresh(joining.url)
        refused = stranger.connect(stranger.own_id(), check=False)
        require(not refused["ok"] and "target_not_member" in refused["error"],
                "a Gateway refuses to forward to a device outside its personal network")
        passed("cross-entry-forwarding", gateway=joining.device_id, target=target.device_id)

        recovered = fresh(target.url)
        checkpoint = verify_recovery(target, recovered, network)
        passed("root-recovery-waits-and-is-vetoed", epoch=checkpoint["epoch"])
        blank = Node(run, "FRESH", binary, workspace, helper)
        nodes.append(blank)
        fresh_checkpoint = recover_fresh_node(blank, target.backup, target.backup_password)
        passed("fresh-node-recovery", network=network, epoch=fresh_checkpoint["epoch"])
        approved = verify_recovery_approval(target, recovered, network)
        passed("root-recovery-manager-confirmation", epoch=approved["epoch"])
        report["status"] = "passed"
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        (run / "never-release").touch()
        for client in reversed(clients):
            client.close()
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        stop_fixture_processes(run)
        report["cleanup"] = {"remaining_nodes": sum(node.process is not None for node in nodes),
                             "remaining_clients": sum(client.process is not None for client in clients)}
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


class PkarrRelay:
    def __init__(self, *, unavailable=False):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        self.packets = {}
        self.reads = 0
        self.writes = 0
        self.lock = threading.Lock()
        relay = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def key(self):
                match = re.fullmatch(r"/pkarr/([a-z0-9]{52})", self.path)
                return match[1] if match else None

            def do_PUT(self):
                key = self.key()
                length = int(self.headers.get("Content-Length", "0"))
                if key is None or not 72 <= length <= 1072:
                    self.send_error(400)
                    return
                packet = self.rfile.read(length)
                with relay.lock:
                    relay.writes += 1
                    if not unavailable:
                        relay.packets[key] = packet
                self.send_response(503 if unavailable else 200)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def do_GET(self):
                with relay.lock:
                    relay.reads += 1
                    packet = relay.packets.get(self.key())
                if unavailable:
                    self.send_error(503)
                    return
                if packet is None:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(len(packet)))
                self.end_headers()
                self.wfile.write(packet)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}/pkarr"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


class DeliveryRpcSpy:
    def __init__(self):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        self.calls = 0
        spy = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                spy.calls += 1
                self.rfile.read(min(int(self.headers.get("Content-Length", "0")), 4096))
                body = b'{"error":"unexpected chain RPC in a free delivery"}'
                self.send_response(502)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def free_delivery_settings(spy, profile, *, enabled):
    return {"enabled": enabled, "charity_enabled": True, "paid_enabled": False,
            "chain": {"rpc_url": spy.url, "chain_id": 31337, "escrow": "0x" + "11" * 20,
                      "asset": "0x" + "22" * 20, "confirmations": 2,
                      "profile": json.loads(profile.read_text())}, "prover": None}


def isolate_delivery_node(node):
    config = json.loads((node.directory / "config.json").read_text())
    config["manager"] = None
    config["providers"]["execution"] = []
    config["triage"]["scan_codex_sessions"] = False
    private_json(node.directory / "config.json", config)


def delivery_rows(node, query, parameters=()):
    database = node.directory / "delivery/service.sqlite3"
    if not database.exists():
        return []
    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as connection:
        return connection.execute(query, parameters).fetchall()


def delivery_entry_args():
    address = f"127.0.0.1:{available_port()}"
    return ["--public-entry", "--relay-listen", address, "--relay-url", "http://" + address]


def delivery_free_state(nodes, spy):
    wallets = sum((node.directory / "delivery-wallet.json").exists() for node in nodes)
    transactions = sum((delivery_rows(node, "SELECT count(*) FROM transactions") or [(0,)])[0][0]
                       for node in nodes)
    require(wallets == 0 and transactions == 0 and spy.calls == 0,
            "free discovery and charity create no wallet, transactions or chain RPC calls")
    return {"wallet_files": wallets, "transaction_rows": transactions, "chain_rpc_calls": spy.calls}


def delivery_call(node, op, **params):
    def attempt():
        response = node.client.command("request", method="delivery", params={"op": op, **params})
        error = response.get("error")
        if error == "another delivery operation is in progress" and op in ("confirm", "accept", "reconcile"):
            lookup = ({"op": "find_intent", "intent_id": params["intent_id"]} if op == "confirm"
                      else {"op": "get", "order_id": params["order_id"]})
            node.client.call("delivery", lookup)
            return None
        require(not error, f"encrypted delivery {op} rejected: {error}")
        return {"result": response.get("result")}
    return wait_for("delivery authority after an explicit pre-mutation busy rejection", attempt)["result"]


def delivery_probe(options, run, actor, targets, order_id, *, action=None):
    probe = options.delivery_probe.resolve(strict=True)
    source = options.delivery_source_root.resolve(strict=True)
    value = {"actor_peer": actor.device_id, "identity_file": str(actor.directory / "identity.json"),
             "targets": [{"peer": node.device_id, "address": f"127.0.0.1:{node.quic_port}",
                          "public_entry": node is targets[-1] and node.name == "P"} for node in targets],
             "order_id": order_id}
    if action is not None:
        value["action"] = action
    path = run / ("probe-" + uuid.uuid4().hex[:10] + ".json")
    private_json(path, value)
    try:
        result = subprocess.run([str(probe), str(path)], capture_output=True, timeout=150)
    finally:
        path.unlink(missing_ok=True)
    with (run / "probe-output.log").open("ab") as output:
        output.write(result.stderr[:16384])
        output.write(result.stdout[:16384])
    require(result.returncode == 0, "real loopback QUIC/Noise probe succeeds; inspect the private probe log")
    public = json.loads(result.stdout)
    require(public.get("passed") is True, "probe returned a positive bounded result")
    if action is None:
        require(public["discovery"]["productSourceSha256"] == file_sha256(source / "crates/node/src/net/pkarr.rs"),
                "probe resolves with the current product discovery implementation")
    return public


def delivery_charity_acceptance(options):
    repository = Path(__file__).resolve().parents[1]
    binary, module = options.binary.resolve(strict=True), options.crypto_module.resolve(strict=True)
    profile = options.delivery_profile.resolve(strict=True)
    artifacts = options.artifacts.resolve()
    require(artifacts.is_relative_to(repository / ".artifacts"), "delivery fixtures stay in the task worktree")
    require(options.delivery_probe is not None, "delivery acceptance requires the real loopback QUIC/Noise probe")
    artifacts.mkdir(parents=True, exist_ok=True)
    run = artifacts / ("delivery-charity-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    relay, fault_directory, spy = PkarrRelay(), PkarrRelay(unavailable=True), DeliveryRpcSpy()
    nodes = []
    report = {"status": "running", "mode": "charity", "source_binary": str(binary),
              "binary_sha256": file_sha256(binary), "profile_sha256": file_sha256(profile),
              "cases": {}, "not_checked": ["paid proof and settlement", "rendered browser UI",
                                             "native mobile UI", "public internet or long-term retention"]}

    def passed(name, **evidence):
        report["cases"][name] = evidence
        print("PASS delivery-charity " + name, flush=True)

    call = delivery_call

    def reconnect(node):
        node.client.connect()
        node.client.authenticate()

    def restart(node):
        node.stop(crash=True)
        node.start()
        if node.client.network_id:
            reconnect(node)

    def no_private_payload(provider, original_name, original):
        name_bytes = original_name.encode()
        marker = original[:64]
        for path in provider.directory.rglob("*"):
            if path.is_file() and not path.is_symlink():
                data = path.read_bytes()
                require(name_bytes not in data and marker not in data,
                        "public provider stores neither original file metadata nor plaintext in data or logs")
        database = provider.directory / "node.sqlite3"
        if database.exists():
            with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as connection:
                tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
                if "events" in tables:
                    require(connection.execute("SELECT count(*) FROM events").fetchone()[0] == 0,
                            "public provider has no private event DAG")

    try:
        for name in ("S", "R", "P", "X"):
            args = delivery_entry_args() if name == "P" else []
            node = Node(run, name, binary, workspace, helper, init_args=args)
            nodes.append(node)
            isolate_delivery_node(node)
            node.configure_network(pkarr={"publish": True, "resolve": True,
                                             "urls": [fault_directory.url, relay.url], "publish_direct": True})
            private_json(node.directory / "delivery-settings.json", free_delivery_settings(spy, profile, enabled=False))
            node.start()
        source, receiver, provider, outsider = nodes
        source.bootstrap()
        receiver.bootstrap()
        require(source.client.network_id != receiver.client.network_id, "source and receiver use different personal networks")
        initial_members = {node.name: sorted(member["device_id"] for member in node.client.call("directory")["view"]["members"])
                           for node in (source, receiver)}
        for node in (source, receiver):
            state = call(node, "status")
            require(state["enabled"] is False and state["wallet"]["state"] == "absent",
                    "object delivery starts disabled with no independent wallet")
        wait_for("free peer publication", lambda: relay.writes >= 3)
        passed("default-disabled-and-free-discovery", **delivery_free_state(nodes, spy), published=relay.writes)
        for node in (source, receiver):
            state = call(node, "configure", enabled=True, charity_enabled=True, paid_enabled=False)
            require(state["enabled"] and not state["paid_enabled"], "manager explicitly enables charity")
        provider.stop()
        private_json(provider.directory / "delivery-settings.json", free_delivery_settings(spy, profile, enabled=True))
        provider.start()
        original_name = "private-" + uuid.uuid4().hex + ".bin"
        original = os.urandom(96 * 1024)
        digest = hashlib.sha256(original).hexdigest()
        intent = str(uuid.uuid4())
        request = {"intent_id": intent, "receiver_peer": receiver.device_id,
                   "provider_peer": provider.device_id, "mode": "charity", "acceptance": "recoverable",
                   "name": original_name, "content_base64": base64.b64encode(original).decode(),
                   "max_service_amount": "0", "max_gas_amount": "0"}
        prepared = call(source, "prepare", **request)
        order_id = prepared["order_id"]
        require(prepared["phase"] == "prepared" and len(prepared["windows"]) == 2
                and prepared["payment"]["state"] == "free" and prepared["payment"]["total"] == "0",
                "charity produces two encrypted windows and zero financial terms")
        repeated = call(source, "prepare", **request)
        found = call(source, "find_intent", intent_id=intent)
        require(repeated["order_id"] == order_id and found["order"]["order_id"] == order_id,
                "same durable intent returns the exact prepared order")
        changed = {**request, "content_base64": base64.b64encode(b"different").decode()}
        refusal = source.client.call("delivery", {"op": "prepare", **changed}, status=400)
        require("intent changed" in refusal["error"], "changed content cannot reuse a prepared intent")
        passed("encrypted-two-window-prepare-and-idempotent-intent", order_id=order_id, windows=2,
               same_intent_same_order=True, changed_content_rejected=True)

        source.stop(crash=True)
        body = delivery_rows(source, "SELECT body FROM charity_orders WHERE id=?", (order_id,))
        require(len(body) == 1, "prepared source state survives its process crash")
        signed = json.loads(body[0][0])["order"]
        ciphertext = source.directory / "delivery/objects" / order_id / "0-ciphertext.bin"
        prefix = base64.b64encode(ciphertext.read_bytes()[:8192]).decode()
        action = {"kind": "prefix", "order": signed, "prefix": prefix, "expected_offset": 0}
        before = delivery_probe(options, run, source, [provider], order_id, action=action)
        checkpoint = delivery_rows(provider, "SELECT committed,size,complete FROM objects WHERE order_id=? AND idx=0 AND kind='ciphertext'", (order_id,))
        require(len(checkpoint) == 1 and checkpoint[0][0] == 8192 and checkpoint[0][1] > 8192
                and checkpoint[0][2] == 0, "provider has an actual incomplete durable prefix")
        restart(provider)
        after = delivery_probe(options, run, source, [provider], order_id,
                               action={**action, "expected_offset": 8192})
        require(before["durablePrefix"] == 8192 and after["afterRestart"] and after["staleOffsetRejected"],
                "public provider resumes the committed prefix and rejects stale offsets after SIGKILL")
        source.start()
        reconnect(source)
        require(call(source, "find_intent", intent_id=intent)["order"]["order_id"] == order_id,
                "source restores its existing prepared intent")
        call(source, "confirm", intent_id=intent)
        duplicate = source.client.command("request", method="delivery",
            params={"op": "confirm", "intent_id": intent}, _wait_timeout=5)
        require(not duplicate.get("error") and duplicate["result"]["order_id"] == order_id,
                "immediate duplicate confirmation returns the same durable order even while the executor is busy")
        incoming = wait_for("receiver gets an encrypted offer", lambda:
            next((item for item in call(receiver, "list")["orders"] if item["order_id"] == order_id), None), timeout=60)
        require(incoming["name"] is None and incoming["sha256"] is None, "receiver learns no original metadata before decryption")
        receiver.client.call("delivery", {"op": "read", "order_id": order_id}, status=400)
        passed("provider-prefix-crash-and-source-intent-recovery", committed_prefix=8192,
               stale_offset_rejected=True, immediate_duplicate_confirmation=True)

        outsider.stop()
        security = delivery_probe(options, run, outsider, [source, receiver, provider], order_id)
        require(security["discovery"]["fresh"] and len(security["targets"]) == 3,
                "real signed discovery and service positive controls accompany all boundary rejections")
        passed("real-service-and-private-network-boundaries", **security)
        call(receiver, "accept", order_id=order_id)
        restart(receiver)

        def recovered():
            state = call(receiver, "get", order_id=order_id)
            return state if state["evidence"] == "charity_receipt" else None

        received = wait_for("receiver decrypts and signs the actual recovered file", recovered, timeout=90)
        recovered_file = call(receiver, "read", order_id=order_id)
        data = base64.b64decode(recovered_file["content_base64"], validate=True)
        require(data == original and recovered_file["sha256"] == digest and recovered_file["name"] == original_name
                and hashlib.sha256(data).hexdigest() == digest and recovered_file["bytes"] == len(original),
                "receiver recovers the original file bytes, name, length and SHA-256")
        require(all(not window["decryption_proven"] for window in received["windows"]),
                "charity signature receipt is not labeled as an on-chain decryption proof")
        wait_for("source reconciles the receiver receipt", lambda:
                 call(source, "get", order_id=order_id)["evidence"] == "charity_receipt", timeout=60)
        no_private_payload(provider, original_name, original)
        require(relay.reads > 0 and relay.writes > 0, "delivery resolves PeerIDs through the free signed directory")
        for node in (source, receiver):
            orders = call(node, "list")["orders"]
            require(sum(order["order_id"] == order_id for order in orders) == 1, "retry and restart retain exactly one order")
        passed("receiver-restart-original-file-and-charity-receipt", bytes=len(original), sha256=digest,
               evidence=received["evidence"], provider_plaintext_absent=True, provider_private_events_absent=True)
        passed("free-path-has-no-financial-side-effects", **delivery_free_state(nodes, spy),
               pkarr_reads=relay.reads, pkarr_writes=relay.writes)
        require(fault_directory.reads > 0 and fault_directory.writes > 0 and relay.reads > 0 and relay.writes > 0,
                "actual discovery tries both the unavailable directory and the working signed directory")
        require(initial_members == {node.name: sorted(member["device_id"] for member in node.client.call("directory")["view"]["members"])
                                   for node in (source, receiver)}, "directory failover does not add or merge personal network members")
        passed("free-directory-failover-preserves-membership-and-no-finance", unavailable_reads=fault_directory.reads,
               unavailable_writes=fault_directory.writes, available_reads=relay.reads, available_writes=relay.writes,
               member_counts={name: len(members) for name, members in initial_members.items()}, **delivery_free_state(nodes, spy))
        report["status"] = "passed"
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        relay.close()
        fault_directory.close()
        spy.close()
        private_json(run / "summary.json", report)
        print("Delivery report: " + str(run / "summary.json"), flush=True)
    return report


class DeliveryRpcForwarder:
    def __init__(self, target):
        self.target, self.raw_broadcasts = target, 0
        self.lock = threading.Lock()
        self.approval_barrier = None
        self.read_failure = None
        recorder = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.connection.settimeout(35)
                try:
                    size = self.headers.get("Content-Length", "")
                    require(self.client_address[0] == "127.0.0.1" and self.headers.get("Origin") is None
                            and self.headers.get("Transfer-Encoding") is None and re.fullmatch(r"[0-9]{1,7}", size)
                            and int(size) <= 1024 * 1024, "task Node RPC forwarding remains local and bounded")
                    body = self.rfile.read(int(size))
                    value = json.loads(body)
                    require(isinstance(value, dict) and value.get("jsonrpc") == "2.0" and isinstance(value.get("method"), str),
                            "task Node RPC forwarding preserves one actual JSON-RPC request")
                    with recorder.lock:
                        barrier = recorder.approval_barrier
                        read_failure = recorder.read_failure
                    candidate = value.get("params", [None])[0] if value.get("params") else None
                    if (read_failure and time.monotonic() < read_failure["expires"] and value["method"] == "eth_call"
                            and isinstance(candidate, dict) and candidate.get("to", "").lower() == read_failure["escrow"]):
                        with recorder.lock:
                            read_failure["blocked"] += 1
                        raise AssertionError("the bounded Node transport failure returns no fabricated chain result")
                    if (barrier and value["method"] == "eth_estimateGas" and isinstance(candidate, dict)
                            and candidate.get("from", "").lower() == barrier["wallet"]
                            and candidate.get("to", "").lower() == barrier["asset"]
                            and re.fullmatch(r"0x095ea7b3[0-9a-fA-F]{128}", candidate.get("data", ""))):
                        barrier["reached"].set()
                        barrier["released"].wait(120)
                        raise AssertionError("the controlled pre-approval checkpoint never forwards its abandoned request")
                    if value["method"] == "eth_sendRawTransaction":
                        with recorder.lock:
                            recorder.raw_broadcasts += 1
                    request = urllib.request.Request(recorder.target, body, {"Content-Type": "application/json"})
                    with urllib.request.urlopen(request, timeout=30) as response:
                        output = response.read(1024 * 1024 + 1)
                    require(len(output) <= 1024 * 1024, "actual task chain RPC response remains bounded")
                    status = 200
                except (AssertionError, ValueError, TypeError, OSError):
                    output, status = b'{"error":"task_rpc_forward_failed"}', 502
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(output)))
                self.end_headers()
                self.wfile.write(output)

        self.server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def broadcasts(self):
        with self.lock:
            return self.raw_broadcasts

    def arm_approval(self, wallet, asset):
        require(re.fullmatch(r"0x[0-9a-fA-F]{40}", wallet) and re.fullmatch(r"0x[0-9a-fA-F]{40}", asset),
                "approval barrier pins only its actual source wallet and fixed asset")
        with self.lock:
            require(self.approval_barrier is None, "one pre-approval checkpoint is armed at a time")
            self.approval_barrier = {"wallet": wallet.lower(), "asset": asset.lower(),
                                     "reached": threading.Event(), "released": threading.Event()}
            return self.approval_barrier["reached"]

    def release_approval(self):
        with self.lock:
            if self.approval_barrier:
                self.approval_barrier["released"].set()
                self.approval_barrier = None

    def fail_reads(self, escrow, timeout=180):
        require(re.fullmatch(r"0x[0-9a-fA-F]{40}", escrow) and 0 < timeout <= 180,
                "one bounded transport failure names only the actual escrow")
        with self.lock:
            require(self.read_failure is None, "Node transport failures do not overlap")
            self.read_failure = {"escrow": escrow.lower(), "expires": time.monotonic() + timeout, "blocked": 0}
            return self.read_failure

    def restore_reads(self):
        with self.lock:
            self.read_failure = None

    def close(self):
        self.release_approval()
        self.restore_reads()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


class DeliveryUiControl:
    names = {"web": "VDP paid web sender", "android": "VDP paid native receiver"}
    sequence = ("prepared", "confirmed", "accepted", "offline-visible", "settled-offline", "saved")
    inspections = {"source-partial": ("web", "web-partial-refund.png"),
                   "source-unknown": ("web", "web-unknown-transaction.png"),
                   "receiver-key-before": ("android", "android-key-before.png"),
                   "receiver-key-after": ("android", "android-key-rotated.png")}

    @staticmethod
    def inspection_verified(checks):
        booleans = ("walletLocked", "proofJobsUnchanged", "transactionIntentsUnchanged", "canonicalRestored")
        return (set(checks) == {*booleans, "rawBroadcastDelta"}
                and type(checks["rawBroadcastDelta"]) is int and checks["rawBroadcastDelta"] == 0
                and all(checks[name] is True for name in booleans))

    def __init__(self, path, repository, source_file, nodes, configuration):
        self.path, self.repository, self.source_file = path, repository, source_file
        self.nodes, self.configuration = nodes, configuration
        owner = {name: os.environ.get("VIBEFACE_DELIVERY_UI_" + name, str(value))
                 for name, value in (("UID", os.getuid()), ("GID", os.getgid()))}
        require(all(re.fullmatch(r"[0-9]{1,10}", value) and int(value) < 2 ** 32 for value in owner.values()),
                "UI fixture owner IDs are explicit bounded decimal values")
        self.uid, self.gid = int(owner["UID"]), int(owner["GID"])
        self.token = os.urandom(32).hex()
        self.pairings, self.events = {}, {}
        self.stage, self.observers, self.inspection, self.inspection_ack = "bootstrap", set(), None, None
        self.closed = False
        self.inspection_finished = threading.Event()
        self.lock = threading.RLock()
        self.client_locks = {role: threading.RLock() for role in self.names}
        self.failure = None
        self.finished = threading.Event()
        self.invitations = {role: nodes[role].client.invite() for role in self.names}
        self.digest = file_sha256(source_file)
        self.size = source_file.stat().st_size
        require(self.size == 4096, "ordinary UI acceptance uses one bounded original window")
        self.server = http.server.HTTPServer(("127.0.0.1", 0), self.handler())
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def start(self):
        require(not self.path.exists() and self.path.parent.resolve().is_relative_to(self.repository / ".artifacts"),
                "UI control descriptor is fresh and remains in the task artifacts")
        require(self.source_file.parent == self.path.parent and stat.S_ISREG(self.source_file.lstat().st_mode),
                "UI exposes only its single synthetic source file in the dedicated control directory")
        self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        require(stat.S_ISDIR(self.path.parent.lstat().st_mode), "UI directory is an actual private task directory")
        self.path.parent.chmod(0o700)
        os.chown(self.path.parent, self.uid, self.gid)
        self.host_file(self.source_file, self.source_file.read_bytes())
        self.thread.start()
        self.publish()

    def host_file(self, path, contents):
        require(path in (self.path, self.source_file) and path.parent == self.path.parent and len(contents) <= 8192,
                "only the descriptor and bounded synthetic source file receive the host owner")
        temporary = path.with_name("." + path.name + "." + uuid.uuid4().hex)
        with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as output:
            os.fchown(output.fileno(), self.uid, self.gid)
            output.write(contents)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)

    def publish(self):
        self.host_file(self.path, json.dumps({"schemaVersion": 1, "url": self.url, "token": self.token}).encode())

    def close(self):
        if self.closed:
            return
        self.closed = True
        self.path.unlink(missing_ok=True)
        if self.thread.is_alive():
            self.server.shutdown()
        self.server.server_close()
        if self.thread.is_alive():
            self.thread.join(timeout=5)
        self.invitations.clear()

    def order(self, node, identifier=None):
        rows = delivery_rows(node, "SELECT body FROM orders" if identifier is None else
                             "SELECT body FROM orders WHERE id=?", () if identifier is None else (identifier,))
        orders = [json.loads(row[0]) for row in rows]
        orders = [order for order in orders if identifier is not None or order["view"].get("name") == self.source_file.name]
        require(len(orders) <= 1, "ordinary UI actions preserve one original order")
        return orders[0] if orders else None

    def snapshot(self):
        if self.stage not in ("ordinary", "complete"):
            return None, None, None
        source = self.order(self.nodes["web"])
        receiver, provider = None, None
        if source:
            view = source["view"]
            require(view["role"] == "sender" and view["mode"] == "paid" and view["acceptance"] == "recoverable" and len(view["windows"]) == 1
                    and view["bytes"] == self.size and view["sha256"] == self.digest
                    and view["source_peer"] == self.nodes["web"].device_id
                    and view["receiver_peer"] == self.nodes["android"].device_id
                    and view["provider_peer"] == self.nodes["provider"].device_id and view["payment"]["total"] == "17",
                    "actual ordinary order binds the selected original, peers, mode and exact fees")
            receiver = self.order(self.nodes["android"], view["order_id"])
            provider = self.order(self.nodes["provider"], view["order_id"])
        return source, receiver, provider

    def facts(self):
        source, receiver, provider = self.snapshot()
        complete = bool(receiver and provider and receiver["view"]["evidence"] == "decryption_proven"
                        and all(window["decryption_proven"] for window in receiver["view"]["windows"])
                        and all(provider["view"]["payment"][name] == "17" for name in ("earned", "withdrawn"))
                        and provider["view"]["payment"]["refunded"] == "0")
        return {"schemaVersion": 1, "nodes": {role: {"url": node.url, "peer": node.device_id}
                for role, node in self.nodes.items()}, "sourceFile": str(self.source_file.relative_to(self.repository)),
                "name": self.source_file.name, "bytes": self.size, "sha256": self.digest,
                "serviceCap": self.configuration["max_service_amount"], "gasCap": self.configuration["source_gas_cap_wei"],
                "source": source["view"] if source else None, "receiver": receiver["view"] if receiver else None,
                "provider": provider["view"] if provider else None, "settled": complete,
                "paired": {role: self.pairings.get(role, {}).get("approved", False) for role in self.names},
                "stage": self.stage, "inspection": self.inspection,
                "events": list(self.events), "failure": self.failure}

    def set_stage(self, stage):
        with self.lock:
            require((self.stage, stage) in (("bootstrap", "inspections"), ("inspections", "ordinary")),
                    "UI stages advance once from pairing through inspection to ordinary actions")
            require(self.observers == set(self.names) and self.inspection is None and self.failure is None,
                    "both real paired observers finish each bounded inspection before ordinary actions")
            self.stage = stage

    def begin_inspection(self, value):
        with self.lock:
            require(self.stage == "inspections" and self.inspection is None and value["kind"] in self.inspections,
                    "one finite original-order inspection is active outside ordinary mutations")
            role, screenshot = self.inspections[value["kind"]]
            require(value["role"] == role and value["screenshot"] == screenshot
                    and value["nodePeer"] == self.nodes[role].device_id and value["timeoutMs"] == 120000,
                    "inspection binds its fixed real Node, role, file and bounded deadline")
            self.inspection_finished.clear()
            self.inspection_ack = None
            self.inspection = value

    def end_inspection(self):
        with self.lock:
            require(self.inspection_ack is not None and self.inspection_finished.is_set() and self.failure is None,
                    "the actual UI acknowledges its current rendered inspection")
            result = self.inspection_ack
            self.inspection, self.inspection_ack = None, None
            return result

    def pairing(self, role):
        require(role in self.names, "UI pairing names one of its two fixed clients")
        with self.client_locks[role]:
            if role in self.pairings:
                return self.pairings[role]
            client = self.nodes[role].client
            requests = client.call("join_status")["requests"]
            pending = [request for request in requests if request["name"] == self.names[role]
                       and request["kind"] == "client" and request["status"] in ("pending", "confirming")]
            require(len(pending) <= 1, "one actual browser or mobile client owns its UI pairing")
            if not pending:
                return None
            request = pending[0]
            if request["status"] == "pending":
                client.start_approval(request["join_id"])
                return None
            if not request.get("joiner_nonce"):
                return None
            pairing = {"peer": request["device_id"], "code": client.command("short_code", request=request), "approved": False}
            require(re.fullmatch(r"[0-9a-f]{64}", pairing["peer"]) and re.fullmatch(r"[0-9]{6}", pairing["code"]),
                    "actual UI pairing returns one peer and a six digit safety code")
            self.pairings[role] = pairing
            return pairing

    def approve(self, role, peer, code):
        require(role in self.names, "approval names its fixed UI client")
        with self.client_locks[role]:
            pairing = self.pairings.get(role)
            require(pairing and pairing["peer"] == peer and pairing["code"] == code,
                    "UI and the manager compare the same actual safety code")
            if not pairing["approved"]:
                client = self.nodes[role].client
                pending = client.pending_join(peer)
                require(pending and pending["name"] == self.names[role] and pending.get("joiner_nonce")
                        and client.command("short_code", request=pending) == code,
                        "manage approval binds the revealed original pairing request")
                client.grant(pending, manage=True)
                pairing["approved"] = True
            return {"approved": True, "peer": peer}

    def event(self, request):
        name = request.get("name")
        if name == "observer-ready":
            require(set(request) == {"name", "role"} and request["role"] in self.names,
                    "bootstrap readiness names one fixed real observer")
            with self.lock:
                require(self.stage == "bootstrap" and self.pairings.get(request["role"], {}).get("approved"),
                        "the observer is actually paired before any whole checkpoint")
                self.observers.add(request["role"])
                return {"recorded": name}
        if name == "inspection-observed":
            require(set(request) == {"name", "role", "id", "screenshot", "sha256", "bytes"},
                    "inspection acknowledgements have one exact finite schema")
            with self.lock:
                current = self.inspection
                require(self.stage == "inspections" and current and all(request[key] == current[key]
                        for key in ("role", "id", "screenshot")), "stale or cross-role inspection acknowledgements are rejected")
                require(re.fullmatch(r"[0-9a-f]{64}", request["sha256"]) and type(request["bytes"]) is int
                        and 8 < request["bytes"] <= 10 * 1024 * 1024, "inspection names a bounded actual PNG")
                screenshot = self.path.parent / "public" / current["screenshot"]
                metadata = screenshot.lstat()
                require(stat.S_ISREG(metadata.st_mode) and metadata.st_size == request["bytes"]
                        and file_sha256(screenshot) == request["sha256"]
                        and screenshot.read_bytes()[:8] == b"\x89PNG\r\n\x1a\n",
                        "inspection acknowledgement matches the actual allowlisted rendered screenshot")
                require(self.inspection_ack is None or self.inspection_ack == request,
                        "an idempotent inspection acknowledgement cannot change its screenshot")
                self.inspection_ack = request
                self.inspection_finished.set()
                return {"recorded": name}
        require(name in self.sequence and set(request) == ({"name", "sha256", "bytes"} if name == "saved" else {"name"}),
                "UI events have a fixed bounded schema")
        with self.lock:
            if name == "saved":
                require(request["sha256"] == self.digest and type(request["bytes"]) is int and request["bytes"] == self.size,
                        "the actual Android saved file matches the original length and SHA-256")
            require(self.stage == "ordinary" or self.stage == "complete" and name in self.events,
                    "ordinary acceptance actions are forbidden during bootstrap and inspection")
            if name in self.events:
                return {"recorded": name}
            require(name == self.sequence[len(self.events)], "ordinary UI events follow the actual acceptance sequence")
            source, receiver, provider = self.snapshot()
            require(source is not None and all(self.pairings.get(role, {}).get("approved") for role in self.names),
                    "both real UI clients are paired before ordinary order acceptance")
            identifier = source["view"]["order_id"]
            funded = delivery_rows(self.nodes["web"], "SELECT id FROM transactions WHERE id=?", (identifier + "/fund",))
            if name == "prepared":
                require(source["view"]["phase"] == "prepared" and not source["confirmed"] and not funded,
                        "actual UI quotation precedes any original order funding")
            else:
                require(source["confirmed"] and source["authorized_by"] == self.pairings["web"]["peer"],
                        "ordinary payment was authorized by the actual Web member")
            if self.sequence.index(name) >= self.sequence.index("accepted"):
                require(receiver and receiver["accepted"] and receiver["authorized_by"] == self.pairings["android"]["peer"],
                        "the real Android member accepts the exact receiver order")
            if name in ("settled-offline", "saved"):
                require(self.facts()["settled"], "actual ordinary proof and withdrawal complete before UI file acceptance")
            self.events[name] = {"order_id": identifier, "intent_id": source["view"]["intent_id"],
                                 "monotonic": time.monotonic()}
            if name == "saved":
                self.stage = "complete"
                self.finished.set()
            return {"recorded": name}

    def handler(self):
        control = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def handle_request(self):
                try:
                    self.connection.settimeout(5)
                    require(self.client_address[0] == "127.0.0.1" and self.headers.get("Host") ==
                            f"127.0.0.1:{control.server.server_port}" and self.headers.get("Origin") is None
                            and self.headers.get("Transfer-Encoding") is None,
                            "UI controller accepts only direct local non-browser requests")
                    require(hmac.compare_digest(self.headers.get("Authorization", ""), "Bearer " + control.token),
                            "UI controller requires its single task bearer")
                    require(self.path in ("/facts", "/invitation/web", "/invitation/android", "/pairing", "/approve", "/event", "/abort"),
                            "UI controller exposes only its fixed test operations")
                    length = self.headers.get("Content-Length", "0")
                    require(re.fullmatch(r"[0-9]{1,4}", length) and int(length) <= 1024,
                            "UI controller input remains bounded")
                    if self.command == "GET":
                        require(length == "0" and self.path in ("/facts", "/invitation/web", "/invitation/android"),
                                "UI reads use one fixed bodyless GET")
                        with control.lock:
                            result = control.facts() if self.path == "/facts" else {"invitation": control.invitations[self.path.rsplit("/", 1)[1]]}
                    else:
                        require(self.command == "POST" and self.path in ("/pairing", "/approve", "/event", "/abort")
                                and self.headers.get("Content-Type") == "application/json", "UI mutations have a fixed JSON entry")
                        request = json.loads(self.rfile.read(int(length)))
                        require(isinstance(request, dict), "UI input is one JSON record")
                        with control.lock:
                            if self.path == "/pairing":
                                require(set(request) == {"role"}, "pairing input has one role")
                                result = control.pairing(request["role"])
                            elif self.path == "/approve":
                                require(set(request) == {"role", "peer", "code"}, "approval input names one original peer and code")
                                result = control.approve(**request)
                            elif self.path == "/abort":
                                require(set(request) == {"role"} and request["role"] in control.names, "abort names one UI client")
                                control.failure = request["role"] + "_ui_failed"
                                control.finished.set()
                                control.inspection_finished.set()
                                result = {"aborted": True}
                            else:
                                result = control.event(request)
                    self.respond(200, {"ok": True, "result": result})
                except (AssertionError, KeyError, ValueError, TypeError, OSError):
                    self.respond(400, {"ok": False, "error": "invalid_ui_control_request"})

            def respond(self, status, value):
                body = json.dumps(value).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            do_GET = handle_request
            do_POST = handle_request

        return Handler


def delivery_ui_selfcheck(options):
    root = Path(__file__).resolve().parents[1]
    require(options.artifacts.resolve().is_relative_to(root / ".artifacts"), "UI selfcheck stays in ignored task artifacts")
    run = options.artifacts.resolve() / ("controller-selfcheck-" + uuid.uuid4().hex)
    run.mkdir(mode=0o700, parents=True)
    source_file = run / "ordinary-paid-ui.bin"
    source_file.write_bytes(os.urandom(4096))

    class Fixture:
        def __init__(self, role, index):
            self.directory = run / role
            self.url = f"http://127.0.0.1:{10000 + index}"
            self.device_id = format(index, "064x")
            self.client = self

        def invite(self):
            return "controller-selfcheck-invitation"

    nodes = {role: Fixture(role, index) for index, role in enumerate(("web", "android", "provider"), 1)}
    control = DeliveryUiControl(run / "control.json", root, source_file, nodes,
                                {"max_service_amount": "1000000", "source_gas_cap_wei": "1000000000000000000"})
    verified = {"walletLocked": True, "rawBroadcastDelta": 0, "proofJobsUnchanged": True,
                "transactionIntentsUnchanged": True, "canonicalRestored": True}
    require(control.inspection_verified(verified), "zero broadcasts with all four actual invariants passes the inspection aggregate")
    for name in ("walletLocked", "proofJobsUnchanged", "transactionIntentsUnchanged", "canonicalRestored"):
        require(not control.inspection_verified({**verified, name: False}), "each failed inspection invariant rejects the aggregate")
    for invalid in (1, False):
        require(not control.inspection_verified({**verified, "rawBroadcastDelta": invalid}),
                "a real broadcast or boolean cannot masquerade as a verified zero integer")
    count = 0

    def check(path, *, body=None, token=True, origin=None, expected=400):
        nonlocal count
        headers = {"Content-Type": "application/json"}
        if token:
            headers["Authorization"] = "Bearer " + control.token
        if origin:
            headers["Origin"] = origin
        request = urllib.request.Request(control.url + path, None if body is None else json.dumps(body).encode(), headers)
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                status, value = response.status, json.load(response)
        except urllib.error.HTTPError as error:
            status, value = error.code, json.load(error)
        require(status == expected and value["ok"] == (expected == 200), "pure controller selfcheck observes its expected HTTP boundary")
        count += 1
        return value.get("result")

    try:
        control.start()
        first_inode = control.path.stat().st_ino
        control.publish()
        require(control.path.stat().st_ino != first_inode and control.path.stat().st_uid == control.uid
                and control.path.stat().st_gid == control.gid and (control.path.stat().st_mode & 0o077) == 0,
                "atomic descriptor republication retains its explicit host owner and private mode")
        require(source_file.stat().st_uid == control.uid and source_file.stat().st_gid == control.gid
                and (source_file.stat().st_mode & 0o077) == 0 and control.path.parent.stat().st_uid == control.uid,
                "only the dedicated UI directory and single synthetic fixture receive the host owner")
        facts = check("/facts", expected=200)
        require(facts["source"] is None and facts["bytes"] == 4096 and not facts["settled"]
                and control.token not in json.dumps(facts), "facts contain public values and no task bearer")
        check("/facts", token=False)
        check("/facts", origin="http://127.0.0.1")
        check("/facts?token=forbidden")
        check("/wallet")
        check("/facts", body={})
        check("/invitation/web", expected=200)
        check("/pairing", body={"role": "other"})
        check("/pairing", body={"role": "web", "extra": True})
        check("/approve", body={"role": "web", "peer": "0" * 64, "code": "000000"})
        check("/event", body={"name": "saved", "bytes": 4096, "sha256": control.digest})
        check("/event", body={"name": "prepared", "extra": True})
        check("/event", body={"name": "confirm"})
        check("/abort", body={"role": "other"})
        check("/abort", body={"role": "android"}, expected=200)
        require(control.finished.is_set() and control.failure == "android_ui_failed" and not control.events,
                "abort completes no acceptance stage")
        control.finished.clear()
        control.failure = None
        control.pairings = {role: {"peer": format(index, "064x"), "code": "123456", "approved": True}
                            for index, role in enumerate(("web", "android"), 4)}
        check("/event", body={"name": "observer-ready", "role": "other"})
        check("/event", body={"name": "observer-ready", "role": "web", "extra": True})
        check("/event", body={"name": "observer-ready", "role": "web"}, expected=200)
        check("/event", body={"name": "observer-ready", "role": "android"}, expected=200)
        check("/event", body={"name": "prepared"})
        control.set_stage("inspections")
        inspection = {"id": str(uuid.uuid4()), "kind": "receiver-key-before", "role": "android",
                      "nodePeer": nodes["android"].device_id, "orderId": None, "intentId": None,
                      "keyId": "c" * 64, "keySerial": 1, "expectedPhase": None, "expectedPaymentState": None,
                      "earned": None, "withdrawn": None, "refunded": None, "evidence": None, "windows": [],
                      "screenshot": "android-key-before.png", "timeoutMs": 120000}
        control.begin_inspection(inspection)
        screenshot = run / "public" / inspection["screenshot"]
        screenshot.parent.mkdir(mode=0o700)
        screenshot.write_bytes(b"\x89PNG\r\n\x1a\n" + b"pure-controller-boundary-fixture")
        acknowledgement = {"name": "inspection-observed", "role": "android", "id": inspection["id"],
                           "screenshot": inspection["screenshot"], "sha256": file_sha256(screenshot), "bytes": screenshot.stat().st_size}
        check("/event", body={**acknowledgement, "id": str(uuid.uuid4())})
        check("/event", body={**acknowledgement, "role": "web"})
        check("/event", body={**acknowledgement, "screenshot": "../other.png"})
        check("/event", body={**acknowledgement, "sha256": "f" * 64})
        check("/event", body={**acknowledgement, "bytes": True})
        check("/event", body={"name": "prepared"})
        check("/event", body=acknowledgement, expected=200)
        check("/event", body=acknowledgement, expected=200)
        require(control.end_inspection() == acknowledgement and not control.events,
                "pure inspection acknowledgement cannot authorize any ordinary order action")
        check("/event", body=acknowledgement)
        control.set_stage("ordinary")
        identifier = "a" * 64
        view = {"order_id": identifier, "intent_id": str(uuid.uuid4()), "role": "sender", "mode": "paid",
                "acceptance": "recoverable", "windows": [{"decryption_proven": False}], "bytes": 4096,
                "name": source_file.name, "sha256": control.digest, "phase": "prepared", "evidence": "none",
                "source_peer": nodes["web"].device_id, "receiver_peer": nodes["android"].device_id,
                "provider_peer": nodes["provider"].device_id,
                "payment": {"total": "17", "earned": "0", "withdrawn": "0", "refunded": "0"}}
        synthetic_source = {"view": view, "confirmed": False, "authorized_by": None}

        def synthetic_order(role, value):
            directory = nodes[role].directory / "delivery"
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            with sqlite3.connect(directory / "service.sqlite3") as database:
                database.execute("CREATE TABLE IF NOT EXISTS orders(id TEXT PRIMARY KEY,body TEXT NOT NULL)")
                database.execute("CREATE TABLE IF NOT EXISTS transactions(id TEXT PRIMARY KEY)")
                database.execute("INSERT OR REPLACE INTO orders(id,body) VALUES(?,?)", (identifier, json.dumps(value)))

        invalid = copy.deepcopy(synthetic_source)
        invalid["view"]["acceptance"] = "strict"
        synthetic_order("web", invalid)
        check("/event", body={"name": "prepared"})
        invalid["view"]["acceptance"] = "recoverable"
        invalid["view"]["sha256"] = "b" * 64
        synthetic_order("web", invalid)
        check("/event", body={"name": "prepared"})
        synthetic_order("web", synthetic_source)
        check("/event", body={"name": "prepared"}, expected=200)
        check("/event", body={"name": "prepared"}, expected=200)
        check("/event", body={"name": "confirmed"})
        synthetic_source.update(confirmed=True, authorized_by="f" * 64)
        synthetic_order("web", synthetic_source)
        check("/event", body={"name": "confirmed"})
        synthetic_source["authorized_by"] = control.pairings["web"]["peer"]
        synthetic_order("web", synthetic_source)
        check("/event", body={"name": "confirmed"}, expected=200)
        check("/event", body={"name": "accepted"})
        synthetic_receiver = {"view": copy.deepcopy(view), "accepted": True, "authorized_by": "f" * 64}
        synthetic_receiver["view"]["role"] = "receiver"
        synthetic_order("android", synthetic_receiver)
        check("/event", body={"name": "accepted"})
        synthetic_receiver["authorized_by"] = control.pairings["android"]["peer"]
        synthetic_order("android", synthetic_receiver)
        check("/event", body={"name": "accepted"}, expected=200)
        check("/event", body={"name": "settled-offline"})
        check("/event", body={"name": "offline-visible"}, expected=200)
        check("/event", body={"name": "settled-offline"})
        synthetic_provider = {"view": copy.deepcopy(view)}
        synthetic_provider["view"]["role"] = "provider"
        synthetic_provider["view"]["payment"].update(earned="17", withdrawn="16")
        synthetic_receiver["view"]["evidence"] = "decryption_proven"
        synthetic_receiver["view"]["windows"][0]["decryption_proven"] = True
        synthetic_order("android", synthetic_receiver)
        synthetic_order("provider", synthetic_provider)
        check("/event", body={"name": "settled-offline"})
        synthetic_provider["view"]["payment"]["withdrawn"] = "17"
        synthetic_order("provider", synthetic_provider)
        check("/event", body={"name": "settled-offline"}, expected=200)
        check("/event", body={"name": "saved", "sha256": control.digest, "bytes": True})
        check("/event", body={"name": "saved", "sha256": "f" * 64, "bytes": 4096})
        check("/event", body={"name": "saved", "sha256": control.digest, "bytes": 4096}, expected=200)
        check("/event", body={"name": "saved", "sha256": "f" * 64, "bytes": 4096})
        require(control.finished.is_set() and list(control.events) == list(control.sequence),
                "synthetic state-machine selfchecks reach exactly the six expected event stages")
    finally:
        control.close()
        source_file.unlink()
    require(not control.path.exists(), "UI control teardown removes its bearer descriptor")
    target_calls = []

    class Stub(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            target_calls.append(value["method"])
            output = json.dumps({"jsonrpc": "2.0", "id": value["id"], "result": "0x1"}).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(output)))
            self.end_headers()
            self.wfile.write(output)

    target = http.server.HTTPServer(("127.0.0.1", 0), Stub)
    target_thread = threading.Thread(target=target.serve_forever, daemon=True)
    target_thread.start()
    recorder = DeliveryRpcForwarder(f"http://127.0.0.1:{target.server_port}")
    results = []

    def synthetic_rpc(method, params):
        request = urllib.request.Request(recorder.url, json.dumps({"jsonrpc": "2.0", "id": 1,
            "method": method, "params": params}).encode(), {"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.load(error)

    try:
        require(synthetic_rpc("eth_chainId", [])[0] == 200 and recorder.broadcasts() == 0,
                "pure forwarder selfcheck preserves an actual local read without labeling it a broadcast")
        require(synthetic_rpc("eth_sendRawTransaction", ["0x00"])[0] == 200 and recorder.broadcasts() == 1,
                "pure forwarder selfcheck counts its synthetic RPC method without storing its payload")
        wallet, asset = "0x" + "11" * 20, "0x" + "22" * 20
        reached = recorder.arm_approval(wallet, asset)
        other = {"from": "0x" + "33" * 20, "to": asset, "data": "0x095ea7b3" + "00" * 64}
        require(synthetic_rpc("eth_estimateGas", [other])[0] == 200 and not reached.is_set(),
                "pre-approval barrier never intercepts a different wallet")
        call_count = len(target_calls)
        operation = threading.Thread(target=lambda: results.append(synthetic_rpc("eth_estimateGas", [{**other, "from": wallet}])), daemon=True)
        operation.start()
        require(reached.wait(5) and operation.is_alive() and len(target_calls) == call_count,
                "pure pre-approval barrier really blocks only the fixed Source estimate before forwarding")
        recorder.release_approval()
        operation.join(timeout=5)
        require(results and results[0][0] == 502 and len(target_calls) == call_count and recorder.broadcasts() == 1,
                "abandoned pre-approval request is released as failure and never reaches the synthetic upstream")
        require(synthetic_rpc("eth_estimateGas", [{**other, "from": wallet}])[0] == 200,
                "a fresh request after checkpoint release uses the actual upstream again")
        fault = recorder.fail_reads(asset)
        call_count = len(target_calls)
        require(synthetic_rpc("eth_call", [{"to": asset, "data": "0x00"}, "latest"])[0] == 502
                and fault["blocked"] == 1 and len(target_calls) == call_count and recorder.broadcasts() == 1,
                "the finite Node read fault returns a real transport failure without fabricating a receipt")
        require(synthetic_rpc("eth_call", [{"to": wallet, "data": "0x00"}, "latest"])[0] == 200
                and synthetic_rpc("eth_chainId", [])[0] == 200,
                "the bounded fault leaves other targets and non-call methods untouched")
        fault["expires"] = time.monotonic() - 1
        require(synthetic_rpc("eth_call", [{"to": asset, "data": "0x00"}, "latest"])[0] == 200,
                "an expired transport fault cannot leave a live Node read blocked")
        recorder.restore_reads()
    finally:
        recorder.close()
        target.shutdown()
        target.server_close()
        target_thread.join(timeout=5)
    print(f"PASS pure delivery-ui selfcheck {count} HTTP cases plus forwarding/barrier/owner checks; no Node, UI, chain or proof started", flush=True)


def delivery_paid_acceptance(options):
    require(options.delivery_config is not None,
            "UNVERIFIED paid delivery requires --delivery-config; charity success is not full delivery acceptance")
    configuration_path = options.delivery_config.resolve(strict=True)
    metadata = options.delivery_config.lstat()
    require(stat.S_ISREG(metadata.st_mode) and metadata.st_uid == os.getuid()
            and metadata.st_mode & 0o077 == 0 and metadata.st_size <= 16384,
            "paid acceptance configuration must be an owner-only bounded regular file")
    configuration = json.loads(configuration_path.read_text())
    expected = {"schema_version", "deployment_file", "prover", "max_service_amount", "source_gas_cap_wei",
                "provider_gas_cap_wei", "register_key_gas_cap_wei", "settlement_observer"}
    require(set(configuration) == expected and configuration["schema_version"] == 1,
            "paid acceptance requires the exact schema_version 1 configuration")
    require(sys.platform.startswith("linux"), "UNVERIFIED paid Node proving requires the real Linux cgroup environment")
    repository = Path(__file__).resolve().parents[1]
    deployment_path = Path(configuration["deployment_file"])
    require(deployment_path.is_absolute() and not deployment_path.exists()
            and deployment_path.resolve().is_relative_to(repository / ".artifacts"),
            "paid acceptance requires a fresh deployment output inside this task's ignored artifacts")
    deployment_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    log = deployment_path.with_suffix(".log").open("xb")
    process = subprocess.Popen(["node", str(repository / "contracts/delivery/deploy-local.mjs"),
                               "--listen", "127.0.0.1:0", "--output", str(deployment_path)],
                              stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
    try:
        def deployed():
            if process.poll() is not None:
                raise RuntimeError("local paid deployment process exited; inspect its task-local log")
            try:
                value = json.loads(deployment_path.read_text())
                return value if value.get("rpc_url") else None
            except (FileNotFoundError, json.JSONDecodeError):
                return None
        deployment = wait_for("fresh local delivery contracts and automatic confirmation blocks", deployed, timeout=120)
        return delivery_paid_run(options, configuration, deployment)
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
        log.close()


def delivery_progress(options):
    commit = os.environ.get("VIBEFACE_DELIVERY_PROGRESS_COMMIT", "")
    if options.delivery_ui_control is None or not re.fullmatch(r"[0-9a-f]{40}", commit):
        return lambda stage: None
    runner = Path(__file__).resolve().parents[1] / "scripts/run-delivery-live.py"
    return runpy.run_path(str(runner))["progress_writer"]("paid", commit)


def delivery_paid_run(options, configuration, deployment):
    enter = delivery_progress(options)
    fields = ("rpc_url", "chain_id", "escrow", "asset", "profile", "confirmations")
    chain = {name: deployment[name] for name in fields}
    profile = json.loads(options.delivery_profile.resolve(strict=True).read_text())
    require(chain["chain_id"] == 31337 and chain["confirmations"] == 2 and chain["profile"] == profile,
            "paid acceptance uses the exact local deployment, product profile and two confirmations")
    url = urllib.parse.urlsplit(chain["rpc_url"])
    require(url.scheme == "http" and url.hostname and ipaddress.ip_address(url.hostname).is_loopback
            and url.port and not url.username and not url.password and url.path in ("", "/")
            and not url.query and not url.fragment, "paid acceptance RPC is a literal loopback HTTP origin")
    for name in ("escrow", "asset"):
        require(re.fullmatch(r"0x[0-9a-fA-F]{40}", chain[name]), "deployment contains a bounded contract address")
    prover = configuration["prover"]
    require(set(prover) == {"program", "r0vm", "cgroup", "profile"} and prover["profile"] == profile,
            "paid acceptance prover uses the fixed product profile")
    for name in ("program", "r0vm", "cgroup"):
        require(Path(prover[name]).is_absolute(), "paid acceptance prover paths are explicit absolute paths")
    for name in ("max_service_amount", "source_gas_cap_wei", "provider_gas_cap_wei", "register_key_gas_cap_wei"):
        require(isinstance(configuration[name], str) and re.fullmatch(r"[1-9][0-9]{0,70}", configuration[name]),
                "paid acceptance financial caps are explicit positive decimal strings")
    enter("free-checks")
    free = delivery_charity_acceptance(options)
    require(free["status"] == "passed", "full paid acceptance also requires the free-network safety checks")
    repository = Path(__file__).resolve().parents[1]
    binary, module = options.binary.resolve(strict=True), options.crypto_module.resolve(strict=True)
    artifacts = options.artifacts.resolve()
    run = artifacts / ("delivery-paid-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    export = run / "settlement"
    export.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    principals = run / "paid-principals"
    principals.mkdir(mode=0o700)
    helper = run / "client.mjs"
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    relay, nodes, passwords = PkarrRelay(), [], {}
    chain_recorder = DeliveryRpcForwarder(chain["rpc_url"]) if options.delivery_ui_control is not None else None
    node_chain = {**chain, "rpc_url": chain_recorder.url} if chain_recorder else chain
    inherited_proof = {}
    registration_process = None
    registration_log = None
    key_processes = []
    branch_process, branch_log = None, None
    data_checkpoints, known_locked, lifecycle_actions, proof_catalog = {}, set(), [], {}
    ui_control, inspection_records, process_recoveries = None, [], []
    last_quiesced = None
    report = {"status": "running", "mode": "paid", "binary_sha256": file_sha256(binary),
              "free_checks": free["status"], "deployment": {name: chain[name] for name in fields if name != "profile"},
              "cases": {}, "not_checked": ["public assets", "absolute chain finality", "rendered native mobile UI",
                                             "public internet or long-term retention"]}

    def passed(name, **evidence):
        report["cases"][name] = evidence
        print("PASS delivery-paid " + name, flush=True)

    def rpc(method, params):
        request = urllib.request.Request(chain["rpc_url"], json.dumps({"jsonrpc": "2.0", "id": 1,
            "method": method, "params": params}).encode(), {"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=30) as response:
            body = response.read(1024 * 1024 + 1)
        require(len(body) <= 1024 * 1024, "local chain RPC response is bounded")
        value = json.loads(body)
        require(value.get("id") == 1 and "error" not in value, "local chain RPC returns a matching successful response")
        return value["result"]

    def confirmed(hash_value):
        receipt = rpc("eth_getTransactionReceipt", [hash_value])
        if receipt is None:
            return None
        require(receipt["status"] == "0x1", "controlled local funding transaction succeeds")
        height = int(rpc("eth_blockNumber", []), 16)
        return receipt if height >= int(receipt["blockNumber"], 16) + chain["confirmations"] - 1 else None

    def cli(node, arguments, *, password=None, timeout=30, success=True, error_contains=None):
        descriptor, password_path = None, None
        command = [str(binary), "wallet", "--data-dir", str(node.directory), *arguments]
        try:
            if password is not None:
                password_path = run / ("passphrase-" + uuid.uuid4().hex)
                descriptor = os.open(password_path, os.O_RDWR | os.O_CREAT | os.O_EXCL, 0o600)
                os.write(descriptor, password.encode() + b"\n")
                os.lseek(descriptor, 0, os.SEEK_SET)
                command += ["--passphrase-fd", str(descriptor)]
            result = subprocess.run(command, capture_output=True, timeout=timeout,
                                    pass_fds=(() if descriptor is None else (descriptor,)))
        finally:
            if descriptor is not None:
                os.close(descriptor)
                password_path.unlink(missing_ok=True)
        require((result.returncode == 0) == success, "local wallet command has its expected outcome without exposing its passphrase")
        if error_contains is not None:
            require(not success and error_contains.encode() in result.stderr,
                    "the rejected local wallet operation names the expected authority or proof-lock boundary")
        if success and result.stdout.lstrip().startswith(b"{"):
            return json.loads(result.stdout)
        return None

    def process_fact(pid):
        try:
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            return {"pid": pid, "start": fields[19], "group": int(fields[2]),
                    "exe": Path(f"/proc/{pid}/exe").resolve(strict=True)}
        except FileNotFoundError:
            return None

    def controlled_descendants(parent):
        pending, found = [parent], {}
        while pending:
            pid = pending.pop()
            if pid in found:
                continue
            fact = process_fact(pid)
            if fact is None:
                continue
            found[pid] = fact
            for task in Path(f"/proc/{pid}/task").glob("*"):
                try:
                    pending.extend(int(child) for child in (task / "children").read_text().split())
                except FileNotFoundError:
                    pass
        found.pop(parent, None)
        return found

    def same_process(fact):
        current = process_fact(fact["pid"])
        return current is not None and current["start"] == fact["start"]

    def owns_proof_lock(fact, lock):
        if not same_process(fact):
            return False
        for fd in Path(f"/proc/{fact['pid']}/fd").glob("*"):
            try:
                if fd.resolve(strict=True) == lock:
                    return True
            except FileNotFoundError:
                pass
        return False

    def proof_busy(lock):
        import fcntl
        descriptor = os.open(lock, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        try:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return True
            fcntl.flock(descriptor, fcntl.LOCK_UN)
            return False
        finally:
            os.close(descriptor)

    def transaction(node, identifier):
        def observed():
            rows = delivery_rows(node, "SELECT hash,phase FROM transactions WHERE id=?", (identifier,))
            require(len(rows) == 1 and re.fullmatch(r"0x[0-9a-f]{64}", rows[0][0]),
                    "settlement bundle references a unique actual Node transaction")
            if rows[0][1] in ("reverted", "conflicted", "failed"):
                raise RuntimeError("actual Node settlement transaction reached a terminal failure")
            return rows[0][0] if rows[0][1] == "confirmed" else None
        return wait_for("Node durably observes its settlement transaction confirmed", observed)

    def public_proof(journal, node, relation, index=None, *, destination=None, name=None):
        destination = export if destination is None else destination
        require(len(journal) == 1504 and int.from_bytes(journal[32:64], "big") == relation,
                "exported journal is the canonical 47-word relation")
        digest = hashlib.sha256(journal).hexdigest()
        rows = delivery_rows(node, "SELECT path FROM proof_jobs WHERE digest=?", (digest,))
        require(len(rows) == 1, "public proof has one durable Node receipt")
        receipt = Path(rows[0][0])
        require(receipt.resolve().is_relative_to(node.directory.resolve()) and stat.S_ISREG(receipt.lstat().st_mode)
                and receipt.stat().st_size <= 16384, "receipt is a bounded regular file in its controlled Node")
        verified = subprocess.run([prover["program"], "verify", journal.hex()], input=receipt.read_bytes(),
                                  capture_output=True, timeout=120,
                                  env={"PATH": "/usr/bin:/bin", "RISC0_SERVER_PATH": prover["r0vm"]})
        require(verified.returncode == 0, "actual host verifies the binary Groth16 receipt against the public journal")
        value = json.loads(verified.stdout)
        image = bytes(profile["guest_image_id"]).hex()
        require(value["verified"] is True and value["kind"] == "groth16" and value["imageId"] == image
                and value["journalHash"] == digest, "verified receipt binds the product image and exact journal")
        profile_bytes = hashlib.sha256(b"vibeface-delivery-profile-v1").digest() + bytes(profile["guest_image_id"]) + bytes(profile["statement_schema_hash"])
        for field in ("crypto_suite", "max_windows", "max_window_bytes", "max_total_bytes", "max_guest_cycles", "max_prover_memory_bytes"):
            profile_bytes += profile[field].to_bytes(32, "big")
        basename = name if name is not None else "key-binding" if index is None else f"relation-{relation}-window-{index}"
        require(re.fullmatch(r"[a-z0-9-]{1,64}", basename), "public receipt export uses a bounded local basename")
        shutil.copyfile(receipt, destination / (basename + ".receipt"))
        private_json(destination / (basename + ".json"), {"mode": "prove", "kind": "groth16", "relation": relation,
                     "imageId": image, "profileId": hashlib.sha256(profile_bytes).hexdigest(),
                     "journal": journal.hex(), "journalHash": digest, "seal": value["seal"]})
        entry = {"relation": relation, "public": basename + ".json", "receipt": basename + ".receipt"}
        if index is not None:
            entry["windowIndex"] = index
        return entry

    def stopped_nodes(label, *, save=False):
        nonlocal last_quiesced
        require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,79}", label), "whole Node checkpoint has one opaque bounded label")
        descendants = {}
        for node in nodes:
            if node.process is not None:
                cli(node, ["lock"])
                require(cli(node, ["status"])["state"] == "locked", "each task wallet is actually locked before its data checkpoint")
                known_locked.add(node.name)
                descendants.update(controlled_descendants(node.process.pid))
            else:
                require(node.name in known_locked, "an already stopped task Node was explicitly locked before stopping")
            lock = node.directory / "delivery/private/prover.lock"
            require(not lock.exists() or not proof_busy(lock), "a whole Node checkpoint cannot cross an inherited active prover lock")
        for node in nodes:
            node.client.close()
            node.stop()
        require(not any(same_process(fact) for fact in descendants.values()),
                "every known task descendant has stopped before copying any Node data")
        for node in nodes:
            lock = node.directory / "delivery/private/prover.lock"
            require(node.process is None and (not lock.exists() or not proof_busy(lock)),
                    "task Node and inherited proof processes are quiescent at the checkpoint")
            for proof_digest, proof_path in delivery_rows(node, "SELECT digest,path FROM proof_jobs"):
                receipt = Path(proof_path)
                require(re.fullmatch(r"[a-f0-9]{64}", proof_digest) and receipt.resolve().is_relative_to(node.directory.resolve())
                        and stat.S_ISREG(receipt.lstat().st_mode) and 0 < receipt.stat().st_size <= 16384,
                        "quiescent proof reservations all have actual bounded atomically completed receipts")
                fact = (node.name, file_sha256(receipt))
                require(proof_digest not in proof_catalog or proof_catalog[proof_digest] == fact,
                        "whole data restoration never changes or regenerates an already catalogued genuine receipt")
                proof_catalog[proof_digest] = fact
        if save:
            require(label not in data_checkpoints, "whole Node checkpoints are immutable once saved")
            destination = run / "private-node-checkpoints" / label
            destination.mkdir(mode=0o700, parents=True)

            def runtime_entries(directory, names):
                skipped = []
                for name in names:
                    mode = (Path(directory) / name).lstat().st_mode
                    require(stat.S_ISDIR(mode) or stat.S_ISREG(mode) or stat.S_ISSOCK(mode),
                            "task Node data checkpoint contains only regular state, directories and disposable sockets")
                    if stat.S_ISSOCK(mode):
                        skipped.append(name)
                return skipped

            for node in nodes:
                require(node.directory.parent.resolve() == run.resolve() and node.directory.resolve().is_relative_to(repository / ".artifacts"),
                        "whole data checkpoints apply only to this run's disposable Nodes")
                shutil.copytree(node.directory, destination / node.name, ignore=runtime_entries)
            data_checkpoints[label] = destination
        last_quiesced = {"nodesStopped": True, "walletsLocked": True, "proversIdle": True, "nodeDataCheckpoint": label}
        return last_quiesced

    def restore_node_data(label):
        require(label in data_checkpoints and all(node.process is None for node in nodes),
                "restore requires one actual whole checkpoint and fully stopped task Nodes")
        retired = run / "private-node-retired" / (label + "-" + uuid.uuid4().hex)
        retired.mkdir(mode=0o700, parents=True)
        for node in nodes:
            node.client.close()
            source_directory = data_checkpoints[label] / node.name
            restored = run / (".node-restore-" + node.name + "-" + uuid.uuid4().hex)
            shutil.copytree(source_directory, restored)
            os.replace(node.directory, retired / node.name)
            os.replace(restored, node.directory)
            known_locked.add(node.name)

    def start_task_nodes(*, unlock=False, names=("S", "R", "P")):
        for node in nodes:
            if node.name not in names:
                continue
            require(node.process is None, "task Node is stopped before its controlled checkpoint restart")
            node.start()
            require(cli(node, ["status"])["state"] == "locked", "whole checkpoint restart actually starts every wallet locked")
            if node.client.network_id:
                node.client.connect()
                node.client.authenticate()
            if unlock:
                cli(node, ["unlock"], password=passwords[node.name])
                known_locked.discard(node.name)

    def compact_view(view):
        return {"phase": view["phase"], "evidence": view["evidence"],
                "payment": {name: view["payment"][name] for name in ("earned", "withdrawn", "refunded")},
                "windows": [{name: window[name] for name in ("state", "decryption_proven")} for window in view["windows"]]}

    def durable_intents():
        return {node.name: {"transactions": delivery_rows(node, "SELECT id,hash,nonce FROM transactions ORDER BY id"),
                            "proofs": delivery_rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest")}
                for node in nodes}

    def principal_state():
        def digest(value):
            return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()

        result = {"nodes": {}, "events": {}, "work_counts": {}, "job_keys": {}, "chain": {},
                  "broadcasts": chain_recorder.broadcasts()}
        queries = {"orders": "SELECT id,network,intent,role FROM orders ORDER BY id",
                   "charity_orders": "SELECT id,network,intent FROM charity_orders ORDER BY id",
                   "transactions": "SELECT id,chain,wallet,nonce,intent,raw,hash FROM transactions ORDER BY id",
                   "gas_budgets": "SELECT * FROM gas_budgets ORDER BY chain,wallet,budget_group",
                   "proof_jobs": "SELECT * FROM proof_jobs ORDER BY digest",
                   "private_values": "SELECT id,hex(body) FROM private_values ORDER BY id",
                   "objects": "SELECT * FROM objects ORDER BY order_id,idx,kind"}
        for node in nodes:
            tables = {}
            for name, query in queries.items():
                rows = delivery_rows(node, query)
                tables[name] = {"count": len(rows), "sha256": digest(rows)}
            views = []
            for body, in delivery_rows(node, "SELECT body FROM orders ORDER BY id"):
                order = json.loads(body)
                order["view"].pop("updated_at_ms", None)
                views.append(order)
            files = {}
            for name in ("config.json", "delivery-settings.json", "delivery-wallet.json"):
                path = node.directory / name
                metadata = path.lstat()
                require(stat.S_ISREG(metadata.st_mode), "principal snapshots read only actual regular task configuration and keystores")
                files[name] = {"sha256": file_sha256(path), "mode": stat.S_IMODE(metadata.st_mode), "mtime_ns": metadata.st_mtime_ns}
            objects = {}
            for path in sorted((node.directory / "delivery/objects").rglob("*")):
                metadata = path.lstat()
                if stat.S_ISDIR(metadata.st_mode):
                    continue
                require(stat.S_ISREG(metadata.st_mode), "principal snapshots do not traverse nonregular task delivery objects")
                objects[str(path.relative_to(node.directory))] = {"sha256": file_sha256(path), "bytes": metadata.st_size}
            proof_material = {}
            for proof_digest, proof_path, size in delivery_rows(node, "SELECT digest,path,size FROM proof_jobs ORDER BY digest"):
                path = Path(proof_path)
                require(path.resolve().is_relative_to(node.directory.resolve()) and stat.S_ISREG(path.lstat().st_mode),
                        "principal snapshots read only actual immutable proof files of this task Node")
                proof_material[proof_digest] = {"sha256": file_sha256(path), "bytes": path.stat().st_size, "reserved_bytes": size}
            result["nodes"][node.name] = {"tables": tables, "views": digest(views), "files": files, "objects": objects,
                                           "proof_material": proof_material, "wallet": cli(node, ["status"])}
            with sqlite3.connect((node.directory / "node.sqlite3").as_uri() + "?mode=ro", uri=True) as database:
                result["events"][node.name] = database.execute("SELECT network_id,author,seq,hash FROM events ORDER BY network_id,author,seq,hash").fetchall()
                result["work_counts"][node.name] = {name: database.execute("SELECT count(*) FROM " + name).fetchone()[0]
                    for name in ("works", "work_runs", "work_dispatches", "work_stops", "schedules", "questions")}
                result["job_keys"][node.name] = database.execute("SELECT conversation_id,trigger FROM scheduler_jobs ORDER BY conversation_id,trigger").fetchall()
            wallet = node.wallet_address
            result["chain"][node.name] = {"latest_nonce": rpc("eth_getTransactionCount", [wallet, "latest"]),
                "pending_nonce": rpc("eth_getTransactionCount", [wallet, "pending"]),
                "native_balance": rpc("eth_getBalance", [wallet, "latest"]),
                "token_balance": rpc("eth_call", [{"to": chain["asset"], "data": "0x70a08231" + wallet[2:].lower().zfill(64)}, "latest"])}
        result["chain"]["escrow_token_balance"] = rpc("eth_call", [{"to": chain["asset"],
            "data": "0x70a08231" + chain["escrow"][2:].lower().zfill(64)}, "latest"])
        return result

    def principal_digest(value):
        return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()

    def principal_client_boundaries(request, prepared):
        identifier, original_intent = prepared["order_id"], prepared["intent_id"]
        clients, checks = [], []
        nonmember = "this device is not a member of the personal network"
        restricted = "delivery writes and spending require a managing member of your personal network"
        settings = {name: delivery_call(source, "status")[name] for name in ("enabled", "charity_enabled", "paid_enabled")}
        actor_directory = principals / "X"
        actor_directory.mkdir(mode=0o700)
        outsider = Client(source.url, helper, identity_file=actor_directory / "identity.json")
        member = Client(source.url, helper, identity_file=principals / "member.json")
        local = Client(f"http://127.0.0.1:{source.local_port}", helper, identity_file=principals / "local.json")
        foreign = Client(source.url, helper, identity_file=receiver.client.identity_file)
        clients.extend((outsider, member, local, foreign))
        try:
            outsider.connect(source.device_id)
            require(outsider.call("info")["device_id"] == source.device_id
                    and outsider.call("authenticate", {"network_id": source.client.network_id})["principal"] is None,
                    "X reaches the actual encrypted Node without a personal membership principal")
            member.join(source.client, "Restricted paid client", manage=False)
            require(member.principal["source"] == "membership" and member.principal["can_manage"] is False,
                    "the actual signed ordinary member has no paid management authority")
            accessed = subprocess.run([str(binary), "access", "--json", "--enable", "--data-dir", str(source.directory)],
                                      capture_output=True, timeout=20)
            require(accessed.returncode == 0, "the owner issues a real local-session entry without exposing launch material")
            entry = json.loads(accessed.stdout)
            fragment = urllib.parse.parse_qs(urllib.parse.urlsplit(entry["url"]).fragment)
            require(entry["device_id"] == source.device_id and entry.get("local") is True
                    and set(fragment) == {"local", "node"} and fragment["node"] == [source.device_id],
                    "the actual owner launch entry pins only this task Source")
            local_peer = local.own_id()
            exchange = urllib.request.Request(local.url + "/api/local/exchange", json.dumps({"token": fragment["local"][0],
                "node_id": source.device_id, "device_id": local_peer}).encode(), {"Origin": local.url, "Content-Type": "application/json"})
            with urllib.request.urlopen(exchange, timeout=15) as response:
                cookie = response.headers["Set-Cookie"].split(";", 1)[0]
                require(response.status == 200 and json.load(response)["network_id"] == source.client.network_id,
                        "the real one-time local exchange returns this Source network")
            local.connect(source.device_id, origin=local.url, local_cookie=cookie)
            local.authenticate(source.client.network_id)
            require(local.principal["source"] == "local_session" and local.principal["can_manage"] is False
                    and local_peer not in {row["device_id"] for row in source.client.call("directory")["view"]["members"]},
                    "the separate actual local session is not silently a managing member")
            require(receiver.client.principal["source"] == "membership" and receiver.client.principal["can_manage"],
                    "the foreign personal-network actor is an actual manager of R")
            foreign.connect(source.device_id)
            require(foreign.call("authenticate", {"network_id": source.client.network_id})["principal"] is None,
                    "actual R management grants no principal at the independent Source")
            require(delivery_call(source, "get", order_id=identifier)["order_id"] == identifier
                    and delivery_call(source, "prepare", **request)["order_id"] == identifier,
                    "the managing Source successfully reads and retries the complete original prepare request")
            private_json(principals / "paid-principal-input.json", {"source_url": source.url, "source_peer": source.device_id,
                "network": source.client.network_id, "order_id": identifier, "intent_id": original_intent, "configure": settings,
                "helper": str(helper), "outsider_identity": str(outsider.identity_file), "outsider_peer": outsider.own_id()})
            conversation = "paid-principal-security"
            source.client.rpc("create_bot", bot_id="paid-principal-bot", conversation_id=conversation,
                name="Paid principal fixture", instructions="", project={"target_node": source.device_id, "cwd": str(workspace)})
            receipt = source.client.rpc("message", conversation_id=conversation, message_id="paid-security-" + uuid.uuid4().hex,
                text=json.dumps({"label": "paid-principal-security", "paid_delivery_security": True}), report_results=False)
            trigger = receipt["trigger"]
            wait_for("the actual controlled Agent turn is live before its principal baseline", lambda:
                (principals / "agent-ready.json").exists()
                and (scheduler_record(source, source.client.network_id, trigger) or {}).get("state") == "running", timeout=90)
            require(all(cli(node, ["status"])["state"] == "unlocked" for node in nodes),
                    "the principal matrix uses the natural pre-confirm idle point with all wallets actually unlocked")
            before = principal_state()
            payloads = ({"op": "prepare", **request}, {"op": "confirm", "intent_id": original_intent}, {"op": "configure", **settings})
            for role, client, expected in (("outsider", outsider, nonmember), ("member", member, restricted), ("local_session", local, restricted)):
                for payload in payloads:
                    error = client.call("delivery", payload, status=400).get("error")
                    require(isinstance(error, str) and expected in error, "each real principal rejects the valid original paid write for its exact authority boundary")
                    checks.append({"actor": role, "op": payload["op"], "rejection": expected})
            for role, client in (("outsider", outsider), ("foreign_manager", foreign)):
                reads = [{"op": "get", "order_id": identifier}]
                if role == "foreign_manager":
                    reads.append({"op": "confirm", "intent_id": original_intent})
                for payload in reads:
                    error = client.call("delivery", payload, status=400).get("error")
                    require(isinstance(error, str) and nonmember in error, "an unjoined actual identity cannot read or authorize the original private order")
                    checks.append({"actor": role, "op": payload["op"], "rejection": nonmember})
                error = client.rpc("nodes", status=400).get("error")
                require(isinstance(error, str) and nonmember in error, "unjoined identities receive no private Node directory")
                checks.append({"actor": role, "op": "nodes", "rejection": nonmember})
            for role, client in (("member", member), ("local_session", local)):
                require(client.call("delivery", {"op": "status"})["node_peer"] == source.device_id
                        and client.call("delivery", {"op": "get", "order_id": identifier})["intent_id"] == original_intent
                        and client.rpc("nodes").get("nodes"), "same-network ordinary members and actual local sessions retain their allowed read access")
                checks.append({"actor": role, "op": "status-get-nodes", "allowed": True})
            private_json(principals / "agent-run.json", {"run": True})
            agent_result = wait_for("the same valid live Agent finishes its finite authority checks", lambda:
                json.loads((principals / "agent-checked.json").read_text()) if (principals / "agent-checked.json").exists() else None, timeout=90)
            require(agent_result["passed"] and agent_result["valid_token_before_and_after"], "Agent authority includes actual valid token positive controls")
            after = principal_state()
            private_json(principals / "client-before.json", before)
            private_json(principals / "client-after.json", after)
            require(before == after, "all client and live Agent attacks leave finances, wallet files, objects, settings, DAG and work counts unchanged")
            private_json(principals / "agent-finish.json", {"finish": True})
            wait_for("the original controlled Agent reaches its normal terminal scheduler state", lambda:
                (scheduler_record(source, source.client.network_id, trigger) or {}).get("state") == "completed", timeout=60)
            completed = principal_state()
            for name in ("nodes", "work_counts", "job_keys", "chain", "broadcasts"):
                require(completed[name] == after[name], "normal Agent completion adds no delivery money, work, configuration or object effects")
            prior_events = {tuple(row) for row in after["events"]["S"]}
            added = [row for row in completed["events"]["S"] if tuple(row) not in prior_events]
            job = scheduler_record(source, source.client.network_id, trigger)
            with sqlite3.connect((source.directory / "node.sqlite3").as_uri() + "?mode=ro", uri=True) as database:
                events = [json.loads(json.loads(row[0])["canonical_body"]) for item in added
                          for row in database.execute("SELECT envelope FROM events WHERE network_id=? AND hash=?",
                                                      (source.client.network_id, item[3])).fetchall()]
            require(len(added) == len(events) == 1 and events[0]["author"] == source.device_id
                    and events[0]["scope_id"] == conversation
                    and events[0]["body"]["type"] == "message_posted" and events[0]["body"]["role"] == "assistant"
                    and events[0]["body"]["message_id"] == job["message_id"]
                    and events[0]["body"]["text"] == "paid-principal-security-complete"
                    and all(completed["events"][name] == after["events"][name] for name in ("R", "P")),
                    "only the original Agent's exact normal assistant completion event is added outside the attack window")
            passed("paid-client-principal-boundaries", status="passed", order_id=identifier, intent_id=original_intent,
                checks=checks, agent=agent_result, before_sha256=principal_digest(before), after_sha256=principal_digest(after),
                wallets_unlocked=True, raw_broadcast_delta=0, new_orders=0, new_proofs=0, dag_unchanged_during_checks=True,
                manager_positive_controls=True,
                agent_completion={"state": "completed", "new_events": 1, "event_hash": added[0][3], "new_work": 0, "financial_state_unchanged": True})
            return actor_directory, outsider.own_id()
        finally:
            private_json(principals / "agent-finish.json", {"finish": True})
            for client in clients:
                client.close()

    def principal_service_boundaries(identifier, actor_directory, actor_peer):
        for node in nodes:
            cli(node, ["unlock"], password=passwords[node.name])
            known_locked.discard(node.name)
        require(all(cli(node, ["status"])["state"] == "unlocked" for node in nodes),
                "completed-order service boundaries run with every wallet genuinely unlocked")
        before = principal_state()
        grant_path = receiver.directory / "paid-security-read-grant.json"
        class Outsider:
            directory, device_id = actor_directory, actor_peer
        try:
            for node in (source, receiver):
                node.client.close()
                node.stop()
            received = delivery_probe(options, run, receiver, [provider], identifier,
                action={"kind": "paid_security", "scenario": {"role": "receiver", "provider_identity_file": str(provider.directory / "identity.json")}})
            require(stat.S_ISREG(grant_path.lstat().st_mode) and grant_path.stat().st_mode & 0o077 == 0
                    and grant_path.stat().st_size <= 2048
                    and file_sha256(grant_path) == received["details"]["receiver_grant_sha256"],
                    "the private grant fixture is the actual provider response already verified by the real receiver")
            encoded = base64.b64encode((provider.directory / "delivery/objects" / identifier / "0-ciphertext.bin").read_bytes()[:8192])
            require(hashlib.sha256(encoded).hexdigest() == received["details"]["ciphertext_base64_sha256"]
                    and received["details"]["expired_signature_verified"] is True,
                    "the legal receiver read matches the actual complete ciphertext and the expired fixture has a valid provider signature")
            sent = delivery_probe(options, run, source, [provider], identifier, action={"kind": "paid_security", "scenario": {"role": "source"}})
            start_task_nodes(unlock=True, names=("S", "R"))
            outside = delivery_probe(options, run, Outsider(), nodes, identifier,
                action={"kind": "paid_security", "scenario": {"role": "outsider", "receiver_grant": json.loads(grant_path.read_text())}})
            for observation in (received, sent, outside):
                announcement = observation["announcement"]
                require(announcement["paid_enabled"] is True and announcement["provider_wallet"].lower() == provider.wallet_address.lower()
                        and announcement["asset"].lower() == chain["asset"].lower() and announcement["profile"] == profile,
                        "every service boundary has a real enabled paid announcement bound to the same provider, asset and proof profile")
            after = principal_state()
            private_json(principals / "service-before.json", before)
            private_json(principals / "service-after.json", after)
            require(before == after, "wrong service roles, correctly signed expiry and private ALPN attempts change no financial or private state")
            passed("paid-service-grant-and-private-alpn-boundaries", status="passed", order_id=identifier,
                checks={"receiver": received["checks"], "source": sent["checks"], "outsider": outside["checks"]},
                positive_paid_announcement=True, positive_source_and_receiver_order=True, positive_receiver_ciphertext_read=True,
                expired_signature_verified=True, expiry_fixture="resigned-provider-host-unix-seconds",
                private_alpn_targets=3, private_alpns=["rpc", "sync", "blob"], before_sha256=principal_digest(before),
                after_sha256=principal_digest(after), wallets_unlocked=True, raw_broadcast_delta=0, new_orders=0, new_proofs=0,
                financial_objects_settings_dag_work_unchanged=True)
        finally:
            grant_path.unlink(missing_ok=True)

    def inspection_view(node, view):
        key = delivery_call(node, "status")["receiver_key"] if node is receiver else None
        return {"phase": view["phase"] if view else None, "paymentState": view["payment"]["state"] if view else None,
                **{name: view["payment"][name] if view else None for name in ("earned", "withdrawn", "refunded")},
                "evidence": view["evidence"] if view else None,
                "windows": [{"state": window["state"], "decryptionProven": window["decryption_proven"]}
                            for window in view["windows"]] if view else [],
                "receiverKey": {"keyId": key["key_id"], "serial": key["serial"]} if key else None}

    def inspect_rendered(kind, *, view=None):
        require(ui_control is not None and kind in DeliveryUiControl.inspections, "rendered inspection uses the existing paired UI control")
        role, screenshot = DeliveryUiControl.inspections[kind]
        node = source if role == "web" else receiver
        before, broadcasts = durable_intents(), chain_recorder.broadcasts()
        require(all(cli(item, ["status"])["state"] == "locked" for item in nodes),
                "every inspection leaves all independent wallets actually locked")
        observed = inspection_view(node, view)
        if kind in ("source-partial", "receiver-key-after"):
            require(observed["phase"] == "partial" and observed["paymentState"] == "withdrawn"
                    and [observed[name] for name in ("earned", "withdrawn", "refunded")] == ["12", "12", "17"]
                    and observed["evidence"] == "none"
                    and observed["windows"] == [{"state": "earned", "decryptionProven": True},
                                               {"state": "refunded", "decryptionProven": False}],
                    "partial screenshots require the actual earned first window and refunded second window")
        if kind == "source-unknown":
            require(observed["phase"] == "reconciling" and observed["paymentState"] == "unknown"
                    and observed["evidence"] == "none"
                    and [observed[name] for name in ("earned", "withdrawn", "refunded")] == ["12", "12", "17"]
                    and observed["windows"] == [{"state": "pending", "decryptionProven": False}] * 2,
                    "unknown screenshots require real retracted completion and proof claims with historical amounts")
        if role == "android":
            require(observed["receiverKey"] and observed["receiverKey"]["serial"] == (1 if kind == "receiver-key-before" else 2),
                    "key screenshots show the actual first registration or sole second-serial rotation")
        value = {"id": str(uuid.uuid4()), "kind": kind, "role": role, "nodePeer": node.device_id,
                 "orderId": view["order_id"] if view else None, "intentId": view["intent_id"] if view else None,
                 "keyId": observed["receiverKey"]["keyId"] if observed["receiverKey"] else None,
                 "keySerial": observed["receiverKey"]["serial"] if observed["receiverKey"] else None,
                 "expectedPhase": observed["phase"], "expectedPaymentState": observed["paymentState"],
                 **{name: observed[name] for name in ("earned", "withdrawn", "refunded", "evidence", "windows")},
                 "screenshot": screenshot, "timeoutMs": 120000}
        ui_control.begin_inspection(value)
        require(ui_control.inspection_finished.wait(120), "the fixed real UI renders and saves its bounded inspection")
        acknowledgement = ui_control.end_inspection()
        require(before == durable_intents() and chain_recorder.broadcasts() == broadcasts
                and all(cli(item, ["status"])["state"] == "locked" for item in nodes),
                "real UI inspection creates no transaction intent, proof reservation or signed broadcast")
        actual = delivery_call(node, "get", order_id=view["order_id"]) if view else None
        require(inspection_view(node, actual) == observed, "the screenshot interval retains the same actual Node projection")
        record = {name: value[name] for name in ("id", "kind", "role", "nodePeer", "orderId", "intentId", "screenshot")}
        record.update(sha256=acknowledgement["sha256"], bytes=acknowledgement["bytes"], observed=observed,
                      checks={"walletLocked": True, "rawBroadcastDelta": 0, "proofJobsUnchanged": True,
                              "transactionIntentsUnchanged": True, "canonicalRestored": kind != "source-unknown"})
        inspection_records.append(record)
        return record

    def view_matches(view, expected):
        phases = ("pending", "held", "key_published", "earned", "refunded")
        return (all(view["payment"][name] == expected[name] for name in ("earned", "withdrawn", "refunded"))
                and len(view["windows"]) == len(expected["windows"])
                and all(local["state"] == phases[remote["phase"]]
                        and local["decryption_proven"] == remote["decryptionProven"]
                        for local, remote in zip(view["windows"], expected["windows"]))
                and (all(window["phase"] == 3 for window in expected["windows"]) or view["phase"] != "paid"))

    def checkpoint_ack(event):
        require(event.get("schemaVersion") == 1 and event.get("noNodeDataCopyByHelper") is True,
                "only the actual finite helper may request a task Node checkpoint")
        checkpoint_id, data_label = event["checkpointId"], event["nodeDataCheckpoint"]
        require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,159}", checkpoint_id) and data_label in data_checkpoints,
                "helper checkpoint names one existing opaque whole Node data label")
        restore_only = any(checkpoint_id.endswith("-" + name) for name in
                           ("restore-data", "restore-main-data", "restore-rotation-data", "restore-C0-data", "case-reset"))
        if restore_only:
            stopped_nodes(data_label)
            restore_node_data(data_label)
            return {"op": "checkpoint-ack", "checkpointId": checkpoint_id, "quiesced": stopped_nodes(data_label),
                    "nodeObservation": "not_observed", "rawBroadcastDelta": 0}
        if checkpoint_id.endswith("-incoming-result"):
            return {"op": "checkpoint-ack", "checkpointId": checkpoint_id, "quiesced": stopped_nodes(data_label),
                    "nodeObservation": "not_observed", "rawBroadcastDelta": 0}
        before = chain_recorder.broadcasts()
        rebuild = checkpoint_id.endswith("-rebuild-C1-data")
        if rebuild or event["nodeMode"] == "owner-native-refund":
            stopped_nodes(data_label)
            restore_node_data(data_label)
        start_task_nodes()
        if event["nodeMode"] == "owner-key-rotate":
            require(not lifecycle_actions, "mixed-held checkpoint produces the sole actual Rotate")
            key_directory = run / "key-lifecycle"
            key_directory.mkdir(mode=0o700)
            rotated = genuine_key_action("rotate", key_directory, order_id, digest)
            lifecycle_actions.append(rotated)
            quiesced = stopped_nodes("AfterRotateData", save=True)
            return {"op": "checkpoint-ack", "checkpointId": checkpoint_id, "quiesced": quiesced,
                    "nodeObservation": "not_observed", "rawBroadcastDelta": chain_recorder.broadcasts() - before,
                    "nodeTransactions": [rotated["public"]["transactionHash"]]}
        if event["nodeMode"] == "owner-native-refund":
            fund_rows = delivery_rows(source, "SELECT hash,phase FROM transactions WHERE id=?", (order_id + "/fund",))
            require(data_label != "C0PrimeData" and len(fund_rows) == 1 and fund_rows[0][1] == "confirmed"
                    and not delivery_rows(provider, "SELECT id FROM transactions WHERE id LIKE ?", (order_id + "/%",)),
                    "actual autonomous refund restores the whole post-fund pre-accept checkpoint with no later provider signed intents")
            restore_expected = event.get("expected")
            require(isinstance(restore_expected, dict), "actual Source refund checkpoint supplies its public expected ledger")
            prior = {row[0] for row in delivery_rows(source, "SELECT id FROM transactions")}
            cli(source, ["unlock"], password=passwords["S"])
            known_locked.discard("S")
            delivery_call(source, "reconcile", order_id=order_id)

            def refunded_source():
                view = delivery_call(source, "get", order_id=order_id)
                return view if all(view["payment"][name] == restore_expected[name] for name in ("earned", "withdrawn", "refunded")) else None

            actual = wait_for("the actual Source independently executes its authorized refund", refunded_source, timeout=120)
            fresh = delivery_rows(source, "SELECT id,hash,phase FROM transactions ORDER BY nonce")
            refunds = [(identifier, hash_value) for identifier, hash_value, phase in fresh
                       if identifier not in prior and phase == "confirmed" and identifier.startswith(order_id + "/refund")]
            require(refunds and len(refunds) <= 4 and len(refunds) == len([row for row in fresh if row[0] not in prior]),
                    "actual Source refund adds only confirmed authorized refund transactions")
            quiesced = stopped_nodes("NativeRefundData-" + uuid.uuid4().hex[:10], save=True)
            return {"op": "checkpoint-ack", "checkpointId": checkpoint_id, "quiesced": quiesced,
                    "nodeObservation": "observed", "rawBroadcastDelta": chain_recorder.broadcasts() - before,
                    "views": {"S": compact_view(actual)}, "nodeTransactions": [hash_value for _, hash_value in refunds]}
        require(event["nodeMode"] == "locked-observe", "ordinary branch observation never unlocks a task wallet")
        expected = event["state"]["order"]
        only_receiver = any(checkpoint_id.endswith("-" + point) for point in
                            ("mixed-held-result", "revoke-observed", "register-observed", "rotate-observed"))
        observed = {}
        for node in ((source,) if rebuild else (receiver,) if only_receiver else (source, receiver)):
            delivery_call(node, "reconcile", order_id=order_id)
            view = wait_for("the real locked Node projection converges to its canonical branch", lambda:
                            (value if view_matches(value, expected) else None)
                            if (value := delivery_call(node, "get", order_id=order_id)) else None, timeout=60)
            observed[node.name] = compact_view(view)
        recovered_sha = None
        if not rebuild:
            recovered_file = delivery_call(receiver, "read", order_id=order_id)
            recovered_bytes = base64.b64decode(recovered_file["content_base64"], validate=True)
            recovered_sha = hashlib.sha256(recovered_bytes).hexdigest()
            require(recovered_bytes == original and len(recovered_bytes) == max_file_bytes
                    and recovered_file["sha256"] == recovered_sha == digest,
                    "the actual warm receiver returns every original byte across branch observation")
        crash_point = ("held" if event["case"] == "held-timeout" and checkpoint_id.endswith("-held") else
                       "key-published" if event["case"] == "strict-late-open" and checkpoint_id.endswith("-key-published") else None)
        if crash_point:
            def provider_projection():
                rows = delivery_rows(provider, "SELECT body FROM orders WHERE id=?", (order_id,))
                value = json.loads(rows[0][0])["view"] if len(rows) == 1 else None
                return value if value and view_matches(value, expected) else None

            provider_before = wait_for("the live provider projects the actual pre-crash canonical stage", provider_projection, timeout=60)
            immutable = durable_intents()
            killed = []
            for node in (receiver, provider):
                require(node.process is not None and node.process.poll() is None and cli(node, ["status"])["state"] == "locked",
                        "only the two live task Nodes are killed at the observed held or key-published stage")
                pid = node.process.pid
                outcome = node.stop(crash=True)
                require(outcome == -signal.SIGKILL, "the real held-stage task process exits from SIGKILL")
                killed.append({"role": node.name, "pid": pid, "signal": "SIGKILL", "exit_code": outcome, "wallet_locked": True})
            start_task_nodes(names=("R", "P"))
            delivery_call(receiver, "reconcile", order_id=order_id)
            after_crash = wait_for("the actual killed receiver converges again to its canonical chain stage", lambda:
                                   (view if view_matches(view, expected) else None)
                                   if (view := delivery_call(receiver, "get", order_id=order_id)) else None, timeout=60)
            provider_after = wait_for("the killed provider projects the same actual canonical stage after restart", provider_projection, timeout=60)
            require(all(cli(node, ["status"])["state"] == "locked" for node in (receiver, provider))
                    and immutable == durable_intents(), "real SIGKILL recovery preserves locked wallets, proof rows and signed transaction intents")
            recovered_file = delivery_call(receiver, "read", order_id=order_id)
            recovered_bytes = base64.b64decode(recovered_file["content_base64"], validate=True)
            require(recovered_bytes == original and len(recovered_bytes) == max_file_bytes
                    and hashlib.sha256(recovered_bytes).hexdigest() == recovered_file["sha256"] == digest
                    and observed["R"]["payment"]["earned"] == after_crash["payment"]["earned"] == expected["earned"]
                    and provider_before["payment"]["earned"] == provider_after["payment"]["earned"] == expected["earned"],
                    "cached old-key original bytes and earned balance survive the actual receiver and provider kills")
            observed["R"] = compact_view(after_crash)
            process_recoveries.append({"point": crash_point, "order_id": order_id, "nodes": killed,
                "earned_before": expected["earned"], "earned_after": after_crash["payment"]["earned"],
                "proof_rows_unchanged": True, "transaction_rows_unchanged": True, "original_sha256": digest,
                "original_bytes": max_file_bytes, "raw_broadcast_delta": 0, "warm_cached_material": True})
            report["process_recoveries"] = process_recoveries
        if event["case"] == "partial-refund" and checkpoint_id.endswith("-result"):
            inspect_rendered("source-partial", view=delivery_call(source, "get", order_id=order_id))
            fault = chain_recorder.fail_reads(chain["escrow"])
            unknown_record = None
            try:
                delivery_call(source, "reconcile", order_id=order_id)

                def source_unknown():
                    view = delivery_call(source, "get", order_id=order_id)
                    return view if view["phase"] == "reconciling" and view["payment"]["state"] == "unknown" else None

                actual_unknown = wait_for("a real bounded Node RPC transport failure retracts the original Source claim", source_unknown, timeout=45)
                require(fault["blocked"] > 0, "the real Node actually encounters the bounded escrow transport failure")
                unknown_record = inspect_rendered("source-unknown", view=actual_unknown)
            finally:
                chain_recorder.restore_reads()
                for node in (source, receiver):
                    delivery_call(node, "reconcile", order_id=order_id)
                    wait_for("removing the transport fault restores the actual canonical order", lambda:
                             view_matches(delivery_call(node, "get", order_id=order_id), expected), timeout=60)
            require(unknown_record is not None, "unknown UI inspection completed before canonical recovery")
            unknown_record["checks"]["canonicalRestored"] = True
        if checkpoint_id.endswith("-mixed-held-result"):
            inspect_rendered("receiver-key-after", view=delivery_call(receiver, "get", order_id=order_id))
        if not only_receiver and not rebuild:
            for node in (source, receiver):
                node.client.close()
                node.stop()
            remote = delivery_probe(options, run, source, [provider], order_id, action={"kind": "paid_order"})["order"]
            require(view_matches(remote, expected), "actual authorized public provider view converges to its canonical branch")
            observed["P"] = compact_view(remote)
        label = "ObservedData-" + uuid.uuid4().hex[:10]
        quiesced = stopped_nodes(label, save=True)
        require(chain_recorder.broadcasts() == before, "locked actual Node branch observations broadcast no signed transaction")
        return {"op": "checkpoint-ack", "checkpointId": checkpoint_id, "quiesced": quiesced,
                "nodeObservation": "observed", "rawBroadcastDelta": 0, "views": observed,
                **({"readSha256": recovered_sha} if recovered_sha is not None else {})}

    def exercise_line(timeout=300):
        require(branch_process is not None, "finite exercise process was explicitly started in this original deployment")
        require(select.select([branch_process.stdout], [], [], timeout)[0], "finite genuine exercise responds within its bounded command deadline")
        line = branch_process.stdout.readline(65537)
        require(line and len(line) <= 65536, "finite genuine exercise returns one bounded public JSON event")
        value = json.loads(line)
        require(value.get("schemaVersion") == 1, "finite genuine exercise uses the exact public event schema")
        if value.get("event") == "error":
            raise RuntimeError("actual genuine branch helper failed: " + str(value.get("error", "inspect its private task log")))
        return value

    def exercise_command(operation, expected_event, **fields):
        request_id = "vdp-" + operation + "-" + uuid.uuid4().hex[:10]
        branch_process.stdin.write(json.dumps({"id": request_id, "op": operation, **fields}) + "\n")
        branch_process.stdin.flush()
        while True:
            event = exercise_line(timeout=9000 if operation == "lifecycle-held" else 300)
            if event["event"] == "checkpoint":
                ack = checkpoint_ack(event)
                branch_process.stdin.write(json.dumps(ack) + "\n")
                branch_process.stdin.flush()
            else:
                require(event["event"] == expected_event and event.get("id") == request_id,
                        "finite exercise response matches its actual original command")
                return event

    def genuine_key_action(action, destination, old_order_id, old_file_sha256):
        require(action in ("rotate", "revoke", "register"), "key lifecycle uses only its three actual owner actions")
        intent = str(uuid.uuid4())
        arguments = [action + "-key", "--intent-id", intent, "--gas-cap", configuration["register_key_gas_cap_wei"]]
        before_jobs = delivery_rows(receiver, "SELECT digest,path FROM proof_jobs ORDER BY digest")
        before_transactions = delivery_rows(receiver, "SELECT id FROM transactions ORDER BY id")
        cli(receiver, ["unlock"], password=passwords["R"])
        active = {}

        def launch():
            attempt = uuid.uuid4().hex
            stdout_path, stderr_path = run / f"key-{action}-{attempt}.stdout", run / f"key-{action}-{attempt}.stderr"
            with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
                process = subprocess.Popen([str(binary), "wallet", "--data-dir", str(receiver.directory), *arguments],
                                           stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True)
            key_processes.append(process)
            active.update(process=process, stdout=stdout_path, stderr=stderr_path)

        def completed():
            process = active["process"]
            if process.poll() is None:
                return None
            stderr = active["stderr"].read_bytes()
            require(len(stderr) <= 65536, "private owner key-operation error remains bounded")
            if process.returncode != 0 and b"another delivery operation is in progress; retry this same key intent" in stderr:
                launch()
                return None
            require(process.returncode == 0, "actual owner key operation succeeds; inspect its private task log on failure")
            output = active["stdout"].read_bytes()
            require(0 < len(output) <= 16384, "actual owner key-operation result remains bounded")
            value = json.loads(output)
            require(value["intent_id"] == intent and value["action"] == action, "actual key response retains its original intent and action")
            return value

        launch()
        first = wait_for("the actual new key proof and owner transaction complete", completed, timeout=7400)

        def confirmed_key():
            value = cli(receiver, arguments, timeout=15)
            if value["phase"] in ("reverted", "conflicted", "failed", "superseded"):
                raise RuntimeError("actual key lifecycle operation failed or was superseded before confirmation")
            require(value["serial"] == first["serial"] and value["binding_hash"] == first["binding_hash"],
                    "same owner key intent retains its exact genuine certificate")
            return value if value["phase"] == "confirmed" and value.get("registry_confirmed") is True else None

        result = wait_for("the actual lifecycle certificate is confirmed in the registry", confirmed_key, timeout=120)
        transaction_id = f"key-{intent}/register"
        rows = delivery_rows(receiver, "SELECT intent FROM transactions WHERE id=?", (transaction_id,))
        require(len(rows) == 1, "each real lifecycle intent has exactly one original registry transaction")
        calldata = bytes.fromhex(json.loads(rows[0][0])["calldata"].removeprefix("0x"))
        journal = calldata[4:1508]
        after_jobs = delivery_rows(receiver, "SELECT digest,path FROM proof_jobs ORDER BY digest")
        require(len(after_jobs) == len(before_jobs) + 1 and set(before_jobs).issubset(after_jobs)
                and len(delivery_rows(receiver, "SELECT id FROM transactions ORDER BY id")) == len(before_transactions) + 1,
                "each actual key lifecycle action adds exactly one genuine proof and one registry transaction")
        entry = public_proof(journal, receiver, 1, destination=destination, name=action)
        transaction_hash = transaction(receiver, transaction_id)
        require(result["tx_hash"] == transaction_hash and type(result["serial"]) is int and result["serial"] > 0
                and re.fullmatch(r"[a-f0-9]{64}", result["binding_hash"]),
                "lifecycle public identifiers come from the actual confirmed result and ledger")
        cli(receiver, ["lock"])
        receiver.client.close()
        receiver.stop(crash=True)
        receiver.start()
        receiver.client.connect()
        receiver.client.authenticate()
        require(cli(receiver, ["status"])["state"] == "locked", "actual lifecycle receiver restart keeps its independent wallet locked")
        recovered = delivery_call(receiver, "read", order_id=old_order_id)
        recovered_bytes = base64.b64decode(recovered["content_base64"], validate=True)
        require(recovered_bytes == original and len(recovered_bytes) == max_file_bytes
                and hashlib.sha256(recovered_bytes).hexdigest() == recovered["sha256"] == old_file_sha256,
                "old held-order key material still recovers the original file after actual key lifecycle and process restart")
        jobs_after_restart = delivery_rows(receiver, "SELECT digest,path FROM proof_jobs ORDER BY digest")
        transactions_after_restart = delivery_rows(receiver, "SELECT id FROM transactions ORDER BY id")
        cli(receiver, ["unlock"], password=passwords["R"])
        repeated = cli(receiver, arguments, timeout=15)
        require(repeated["phase"] == "confirmed" and repeated.get("registry_confirmed") is True
                and repeated["serial"] == result["serial"] and repeated["binding_hash"] == result["binding_hash"]
                and repeated["tx_hash"] == transaction_hash
                and delivery_rows(receiver, "SELECT digest,path FROM proof_jobs ORDER BY digest") == jobs_after_restart
                and delivery_rows(receiver, "SELECT id FROM transactions ORDER BY id") == transactions_after_restart,
                "same lifecycle UUID after a real restart reuses its original certificate, proof and transaction")
        if action == "revoke":
            blocked = ["rotate-key", "--intent-id", str(uuid.uuid4()), "--gas-cap", configuration["register_key_gas_cap_wei"]]
            cli(receiver, blocked, success=False,
                error_contains="rotate-key requires a confirmed active receiving key; use register-key to enable receiving")
            require(delivery_rows(receiver, "SELECT digest,path FROM proof_jobs ORDER BY digest") == jobs_after_restart
                    and delivery_rows(receiver, "SELECT id FROM transactions ORDER BY id") == transactions_after_restart,
                    "a fresh rotate intent cannot reactivate revoked receiving or reserve any proof or nonce")
        metadata = json.loads((destination / entry["public"]).read_text())
        public_result = {"action": action, "bindingAction": "revoked" if action == "revoke" else "active",
                         "serial": result["serial"], "intentId": intent, "bindingHash": result["binding_hash"],
                         "transactionId": transaction_id, "transactionHash": transaction_hash, "relation": 1,
                         "public": entry["public"], "receipt": entry["receipt"],
                         "journalHash": hashlib.sha256(journal).hexdigest(), "receiptSha256": file_sha256(destination / entry["receipt"]),
                         "newProofs": 1, "registryConfirmed": True, "retryReusedIntent": True,
                         "restartedWalletLocked": True, "oldOrderReadSha256": recovered["sha256"]}
        return {"public": public_result, "journal": journal, "transaction_id": transaction_id,
                "metadata": {name: metadata[name] for name in ("imageId", "profileId")}}

    def ordinary_ui_acceptance(current_binding_journal, registration_id):
        require(options.delivery_ui_control is not None and type(current_binding_journal) is bytes
                and len(current_binding_journal) == 1504 and isinstance(registration_id, str),
                "ordinary UI starts only with the explicitly supplied current active registration")
        active_transaction = delivery_rows(receiver, "SELECT intent,phase FROM transactions WHERE id=?", (registration_id,))
        require(len(active_transaction) == 1 and active_transaction[0][1] == "confirmed", "ordinary UI uses a confirmed actual key registration")
        active_calldata = bytes.fromhex(json.loads(active_transaction[0][0])["calldata"].removeprefix("0x"))
        require(active_calldata[4:1508] == current_binding_journal, "ordinary UI binding proof matches its exact registration transaction")
        destination = run / "ordinary-settlement"
        destination.mkdir(mode=0o700)
        require(ui_control is not None and ui_control.stage == "inspections", "the ordinary UI reuses the actual members captured before C0")
        original_path = ui_control.source_file
        ordinary_bytes, ordinary_digest = original_path.read_bytes(), file_sha256(original_path)
        before_jobs = {node.name: delivery_rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest") for node in nodes}
        for node in nodes:
            cli(node, ["unlock"], password=passwords[node.name])
        control = ui_control
        try:
            control.set_stage("ordinary")
            print("OBSERVE delivery-paid ordinary Web and Android UI controller ready", flush=True)
            require(control.finished.wait(options.delivery_ui_timeout_ms / 1000), "ordinary UI completes within its explicit acceptance bound")
            require(control.failure is None and list(control.events) == list(control.sequence),
                    "both actual UI clients complete quotation, payment, disconnect, recovery and saved-file acceptance")
            facts = control.facts()
            ordinary_order = facts["source"]
            require(facts["settled"] and ordinary_order is not None, "ordinary UI has actual original-chain settlement evidence")
            ui_events = dict(control.events)
        finally:
            control.close()
        ordinary_id = ordinary_order["order_id"]
        recovered = delivery_call(receiver, "read", order_id=ordinary_id)
        require(base64.b64decode(recovered["content_base64"], validate=True) == ordinary_bytes
                and recovered["sha256"] == ordinary_digest and recovered["bytes"] == 4096,
                "actual Node ordinary recovery matches the UI-selected original bytes")
        ordinary_proofs = [public_proof(current_binding_journal, receiver, 1, destination=destination)]
        source_input = delivery_rows(source, "SELECT body FROM private_values WHERE id=?", (f"source-input:{ordinary_id}:0",))
        require(len(source_input) == 1, "ordinary source proof retains its own actual public statement")
        journal = bytes(json.loads(source_input[0][0])["statement"])
        ordinary_proofs.append(public_proof(journal, source, 2, 0, destination=destination))
        for relation in (3, 4):
            public = journal[:32] + relation.to_bytes(32, "big") + journal[64:]
            ordinary_proofs.append(public_proof(public, receiver, relation, 0, destination=destination))
        after_jobs = {node.name: delivery_rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest") for node in nodes}
        require(sum(len(after_jobs[name]) - len(before_jobs[name]) for name in before_jobs) == 3
                and all(set(before_jobs[name]).issubset(after_jobs[name]) for name in before_jobs),
                "ordinary UI generates exactly three new proofs and retains all previous actual receipts")
        ordinary_transactions = {"registerKey": transaction(receiver, registration_id),
                                 "approve": transaction(source, f"{ordinary_id}/approve"),
                                 "fund": transaction(source, f"{ordinary_id}/fund")}
        for action in ("hold", "publish", "open", "withdraw"):
            identifier = f"{ordinary_id}/{action}-0"
            ordinary_transactions[action] = [transaction(provider, identifier)]
        ordinary_capsule = provider.directory / "delivery/objects" / ordinary_id / "0-capsule.bin"
        bundle = {"schemaVersion": 1, "escrow": chain["escrow"], "asset": chain["asset"], "orderId": "0x" + ordinary_id,
                  "proofs": ordinary_proofs, "capsules": [{"windowIndex": 0, "capsule": ordinary_capsule.read_bytes().hex()}],
                  "transactions": ordinary_transactions}
        ordinary_bundle = destination / "settlement.json"
        private_json(ordinary_bundle, bundle)
        observer = Path(configuration["settlement_observer"]).resolve(strict=True)
        observed = subprocess.run(["node", str(observer), "--settlement", str(ordinary_bundle), "--rpc-url", chain["rpc_url"],
                                   "--case", "ordinary"], capture_output=True, timeout=180)
        with (run / "ordinary-settlement-observer.log").open("wb") as output:
            output.write(observed.stdout[:65536])
            output.write(observed.stderr[:65536])
        require(observed.returncode == 0, "independent ordinary observer verifies four public receipts and actual original-chain payment")
        require(len(observed.stdout) <= 65536, "ordinary public observer receipt remains bounded")
        ordinary_observation = json.loads(observed.stdout)
        require(ordinary_observation.get("status") == "PASS", "ordinary public observer receipt records this actual success")
        private_json(destination / "settlement-observation.json", ordinary_observation)
        passed("ordinary-paid-real-web-android-offline-recovery-and-save", order_id=ordinary_id,
               intent_id=ordinary_order["intent_id"], bytes=4096, sha256=ordinary_digest, windows=1,
               acceptance="recoverable", new_proofs=3, public_proofs=4, current_registration=registration_id,
               earned="17", withdrawn="17", refunded="0", bundle=str(ordinary_bundle), ui_events=ui_events,
               actual_web_confirmation=True, actual_android_acceptance=True, actual_android_saved_file=True,
               offline_visible=True, settled_while_app_background=True, read_only_observer=True)
        report["not_checked"] = [item for item in report["not_checked"] if item != "rendered native mobile UI"]
        return ordinary_bundle

    try:
        enter("wallet-bootstrap")
        accounts = rpc("eth_accounts", [])
        require(accounts and deployment.get("funder", accounts[0]).lower() == accounts[0].lower(),
                "only the controlled fixture's unlocked local funder is used")
        funder = accounts[0]
        require(int(rpc("eth_chainId", []), 16) == 31337, "the running local chain has the pinned identity")
        for name in ("S", "R", "P"):
            args = delivery_entry_args() if name == "P" else []
            node = Node(run, name, binary, workspace, helper, init_args=args)
            nodes.append(node)
            isolate_delivery_node(node)
            if name == "S" and chain_recorder:
                node.configure(timeout_secs=600, manager=fixture_manager_settings(principals, workspace),
                    bins={"codex": str(fixture_wrapper(principals, "fixture-codex", "--fixture-codex"))}, app_server=SPAWN_APP_SERVER)
            node.configure_network(pkarr={"publish": True, "resolve": True, "urls": [relay.url], "publish_direct": True})
            node.start()
            if name != "P":
                node.bootstrap()
            settings = {"enabled": True, "charity_enabled": False, "paid_enabled": True, "chain": node_chain,
                        "prover": prover, "provider_gas_cap": configuration["provider_gas_cap_wei"],
                        "delivery_fee_per_window": "10", "reservation_fee_per_window": "2", "priority_fee": "5"}
            settings_path = node.directory / "acceptance-delivery-settings.json"
            private_json(settings_path, settings)
            cli(node, ["configure", str(settings_path)])
            password = base64.urlsafe_b64encode(os.urandom(32)).decode()
            passwords[name] = password
            created = cli(node, ["create"], password=password)
            require(created["state"] == "locked", "a newly created wallet starts locked")
            cli(node, ["unlock"], password="invalid-acceptance-passphrase", success=False)
            require(cli(node, ["status"])["state"] == "locked", "wrong local passphrase cannot unlock the wallet")
            unlocked = cli(node, ["unlock"], password=password)
            require(unlocked["state"] == "unlocked" and unlocked["address"] == created["address"],
                    "protected FD unlocks only this Node's independent wallet")
            node.wallet_address = unlocked["address"]
            funding = rpc("eth_sendTransaction", [{"from": funder, "to": node.wallet_address,
                                                   "value": hex(10 ** 19)}])
            wait_for("controlled Node native gas funding", lambda: confirmed(funding), timeout=60)
        source, receiver, provider = nodes
        require(source.client.network_id != receiver.client.network_id, "paid transfer does not merge personal networks")
        transfer = "0xa9059cbb" + source.wallet_address[2:].lower().zfill(64) + format(1000000, "064x")
        token_tx = rpc("eth_sendTransaction", [{"from": funder, "to": chain["asset"], "data": transfer}])
        wait_for("controlled sender TestToken funding", lambda: confirmed(token_tx), timeout=60)
        register_intent = str(uuid.uuid4())
        registration_log = (run / "register-before-crash.log").open("wb")
        enter("first-proof-starting")
        registration_process = subprocess.Popen([str(binary), "wallet", "--data-dir", str(receiver.directory),
            "register-key", "--intent-id", register_intent, "--gas-cap", configuration["register_key_gas_cap_wei"]],
            stdin=subprocess.DEVNULL, stdout=registration_log, stderr=registration_log, start_new_session=True)
        lock = (receiver.directory / "delivery/private/prover.lock").resolve()
        actual_r0vm = Path(prover["r0vm"]).resolve(strict=True)

        def genuine_prover_started():
            if registration_process.poll() is not None:
                raise RuntimeError("the first real receiving-key proof stopped before its crash checkpoint; inspect the private registration log")
            facts = controlled_descendants(receiver.process.pid)
            inherited = {pid: fact for pid, fact in facts.items() if owns_proof_lock(fact, lock)}
            r0vm = [fact for fact in inherited.values() if fact["exe"] == actual_r0vm]
            if r0vm and len(inherited) >= 2 and lock.exists() and proof_busy(lock):
                require(all(fact["group"] not in (os.getpgrp(), receiver.process.pid) for fact in inherited.values()),
                        "the trusted proof descendants use a separate process group owned by this task")
                return inherited
            return None

        inherited_proof = wait_for("actual trusted host and r0vm inherit the exclusive Node proof lock", genuine_prover_started, timeout=120)
        enter("first-proof-observed")
        responsiveness = []
        for sample in range(3):
            require(proof_busy(lock) and any(same_process(fact) and owns_proof_lock(fact, lock) for fact in inherited_proof.values()),
                    "the actual trusted prover retains its inherited proof lock before each responsiveness sample")
            for method, read in (("encrypted delivery.status", lambda: delivery_call(receiver, "status")),
                                 ("HTTP GET /api/info", lambda: receiver.client.request("/api/info")),
                                 ("local wallet status", lambda: cli(receiver, ["status"]))):
                started = time.monotonic_ns()
                value = read()
                elapsed = time.monotonic_ns() - started
                require(isinstance(value, dict) and value, "the actual Node completes each sampled status or GET read while proving")
                require((method == "encrypted delivery.status" and value.get("node_peer") == receiver.device_id)
                        or (method == "HTTP GET /api/info" and value.get("device_id") == receiver.device_id)
                        or (method == "local wallet status" and value.get("state") == "unlocked" and value.get("address") == receiver.wallet_address),
                        "each sampled response has its actual pinned Node identity or independent wallet state")
                responsiveness.append({"sample": sample, "method": method, "elapsed_ns": elapsed, "success": True})
            require(proof_busy(lock) and any(same_process(fact) and owns_proof_lock(fact, lock) for fact in inherited_proof.values()),
                    "the actual trusted prover retains its inherited proof lock after each responsiveness sample")
        passed("node-status-and-get-respond-during-genuine-proof", samples=responsiveness, successful_reads=len(responsiveness),
               actual_r0vm_observed_at_start=True, actual_trusted_proof_busy_before_and_after=True, actual_inherited_lock_busy=True,
               clock="monotonic_ns", scope="three actual read methods during the first real key-binding proof; no throughput or baseline comparison")
        jobs = delivery_rows(receiver, "SELECT digest,path FROM proof_jobs")
        require(len(jobs) == 1, "the first receiving-key proof has one reserved stable receipt")
        proof_digest, receipt_name = jobs[0]
        receipt = Path(receipt_name)
        require(receipt.resolve().is_relative_to(receiver.directory.resolve()) and not receipt.exists(),
                "the genuine crash checkpoint precedes publication of its original receipt")
        enter("receiver-crash")
        receiver.stop(crash=True)
        require(any(same_process(fact) and owns_proof_lock(fact, lock) for fact in inherited_proof.values()) and proof_busy(lock),
                "the actual authorized proof retains the lock after its Node is killed")
        try:
            registration_process.wait(timeout=35)
        except subprocess.TimeoutExpired:
            registration_process.terminate()
            registration_process.wait(timeout=5)
        registration_log.close()
        registration_log = None
        receiver.start()
        receiver.client.connect()
        receiver.client.authenticate()
        require(cli(receiver, ["status"])["state"] == "locked", "SIGKILL restores the actual receiving wallet locked")
        registration_args = ["register-key", "--intent-id", register_intent,
                             "--gas-cap", configuration["register_key_gas_cap_wei"]]
        cli(receiver, registration_args, success=False, error_contains="unlock the independent delivery wallet locally first")
        require(proof_busy(lock), "the original proof still occupies its inherited lock at the retry checkpoint")
        cli(receiver, ["unlock"], password=passwords["R"])
        cli(receiver, registration_args, success=False,
            error_contains="a previously authorized proof is still running; waiting to reconcile its receipt")
        cli(receiver, ["lock"])
        spawned = controlled_descendants(receiver.process.pid)
        require(not any(fact["exe"] == actual_r0vm or owns_proof_lock(fact, lock) for fact in spawned.values()),
                "same-intent retries create no second host or r0vm while the inherited proof is active")
        require(delivery_rows(receiver, "SELECT digest,path FROM proof_jobs") == jobs
                and not delivery_rows(receiver, "SELECT id FROM transactions WHERE id=?", (f"key-{register_intent}/register",)),
                "the crash and occupied-lock retries preserve one proof reservation and reserve no transaction nonce")
        print("OBSERVE delivery-paid the original receiving-key proof continues after Node SIGKILL", flush=True)

        def original_receipt_ready():
            if receipt.exists() and not proof_busy(lock):
                metadata = receipt.lstat()
                require(stat.S_ISREG(metadata.st_mode) and 0 < metadata.st_size <= 16384,
                        "the original trusted prover atomically publishes its bounded receipt")
                return True
            require(any(same_process(fact) for fact in inherited_proof.values()),
                    "the authorized prover remains observable until it publishes its receipt")
            return False

        enter("original-receipt-wait")
        wait_for("the original genuine receiving-key proof completes without a duplicate job", original_receipt_ready, timeout=7400)
        enter("original-receipt-ready")
        original_receipt_sha = file_sha256(receipt)
        require(cli(receiver, ["status"])["state"] == "locked", "waiting for the authorized receipt does not unlock the receiving wallet")
        cli(receiver, ["unlock"], password=passwords["R"])

        def registration():
            value = cli(receiver, registration_args, timeout=7300)
            if value["phase"] in ("reverted", "conflicted", "failed"):
                raise RuntimeError("receiving-key registration reached a terminal chain failure")
            return value if value["phase"] == "confirmed" and value.get("registry_confirmed") is True else None
        enter("registration-reconcile")
        wait_for("actual receiving-key proof registration confirmation", registration, timeout=7400)
        require(delivery_rows(receiver, "SELECT digest,path FROM proof_jobs") == jobs
                and file_sha256(receipt) == original_receipt_sha
                and len(delivery_rows(receiver, "SELECT id FROM transactions WHERE id=?", (f"key-{register_intent}/register",))) == 1,
                "same registry intent reuses the original genuine receipt and has exactly one durable transaction")
        enter("registration-confirmed")
        passed("genuine-prover-survives-node-sigkill-without-a-duplicate-job", proof_digest=proof_digest,
               original_receipt_sha256=original_receipt_sha, inherited_lock=True,
               restarted_wallet_locked=True, same_intent=True, proof_reservations=1, registry_transactions=1)
        passed("local-wallets-and-genuine-receiving-key-registration", wallets=3, chain_id=31337,
               two_confirmations=True, protected_passphrase_fd=True)
        if chain_recorder:
            branch_log = (run / "genuine-branches-private.log").open("xb")
            branch_process = subprocess.Popen(["node", str(repository / "contracts/delivery/exercise-settlement.mjs"),
                "--settlement", str(export / "settlement.json"), "--rpc-url", chain["rpc_url"],
                "--deployment", configuration["deployment_file"]], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=branch_log, text=True, bufsize=1, start_new_session=True)
            ready = exercise_line(timeout=120)
            require(ready["event"] == "ready" and ready["chainId"] == 31337 and ready["escrow"].lower() == chain["escrow"].lower()
                    and re.fullmatch(r"[a-f0-9-]{36}", ready["sessionId"]),
                    "finite genuine helper is attached to the same original local deployment")
            branch_session = ready["sessionId"]
            branch_public_index = ready["publicIndex"]
            require(branch_public_index == f".artifacts/vdp/contracts/branch-observations/{branch_session}/index.json",
                    "finite helper identifies this exact session's repository-relative public index")
            wallets = {node.name: node.wallet_address for node in nodes}
            original_path = options.delivery_ui_control.resolve().parent / "ordinary-paid-ui.bin"
            require(original_path.parent.is_relative_to(repository / ".artifacts"), "early UI retains its single bounded synthetic source in task artifacts")
            original_path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            with os.fdopen(os.open(original_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as output:
                output.write(os.urandom(4096))
            for node in nodes:
                cli(node, ["lock"])
                known_locked.add(node.name)
            enter("controller-start")
            ui_control = DeliveryUiControl(options.delivery_ui_control.resolve(), repository, original_path,
                                            {"web": source, "android": receiver, "provider": provider}, configuration)
            ui_control.start()
            enter("controller-published")

            def observers_ready():
                require(ui_control.failure is None, "both actual UI observers remain healthy during pre-C0 pairing")
                return ui_control.observers == set(ui_control.names)

            wait_for("both real UI members pair before the original whole Node checkpoints", observers_ready, timeout=120)
            ui_control.set_stage("inspections")
            inspect_rendered("receiver-key-before")
            quiesced = stopped_nodes("C0Data", save=True)
            exercise_command("capture-pre-prepare", "captured", quiesced=quiesced, wallets=wallets,
                             receiverPeer="0x" + receiver.device_id)
            exercise_command("resume-main", "main-miner-resumed")
            start_task_nodes(unlock=True)
        max_file_bytes = 130_816
        require(delivery_call(source, "status")["max_file_bytes"] == max_file_bytes
                and delivery_call(receiver, "status")["max_file_bytes"] == max_file_bytes,
                "paid acceptance covers the exact advertised original-file capacity")
        original = os.urandom(130_816)
        digest, name, intent = hashlib.sha256(original).hexdigest(), "private-paid-" + uuid.uuid4().hex + ".bin", str(uuid.uuid4())
        request = {"intent_id": intent, "receiver_peer": receiver.device_id, "provider_peer": provider.device_id,
                   "mode": "paid", "acceptance": "strict", "name": name,
                   "content_base64": base64.b64encode(original).decode(), "max_service_amount": configuration["max_service_amount"],
                   "max_gas_amount": configuration["source_gas_cap_wei"]}
        prepared = delivery_call(source, "prepare", **request)
        order_id = prepared["order_id"]
        require(prepared["bytes"] == max_file_bytes and len(prepared["windows"]) == 2
                and prepared["payment"]["total"] == "29",
                "maximum-capacity strict two-window quotation has the exact original length and fees")
        require(delivery_call(source, "prepare", **request)["order_id"] == order_id,
                "same paid intent returns the existing order before authorization")
        changed = {**request, "content_base64": base64.b64encode(b"different").decode()}
        source.client.call("delivery", {"op": "prepare", **changed}, status=400)
        if chain_recorder:
            outsider_directory, outsider_peer = principal_client_boundaries(request, prepared)
            cli(provider, ["lock"])
            require(cli(provider, ["status"])["state"] == "locked", "intentional paid provider outage starts with its actual wallet locked")
            known_locked.add("P")
            provider.client.close()
            provider.stop()
            approval_barrier = chain_recorder.arm_approval(source.wallet_address, chain["asset"])
        delivery_call(source, "confirm", intent_id=intent)
        duplicate = source.client.command("request", method="delivery",
            params={"op": "confirm", "intent_id": intent}, _wait_timeout=5)
        require(not duplicate.get("error") and duplicate["result"]["order_id"] == order_id,
                "immediate duplicate authorization keeps the exact original paid intent")
        if chain_recorder:
            wait_for("both real source proofs finish before the original approval estimate is released", approval_barrier.is_set, timeout=16000)
            require(len(delivery_rows(source, "SELECT digest,path FROM proof_jobs")) == 2
                    and not delivery_rows(source, "SELECT id FROM transactions"),
                    "real C0Prime barrier precedes every Source approval/funding reservation while both source proofs are durable")
            quiesced = stopped_nodes("C0PrimeData", save=True)
            chain_recorder.release_approval()
            exercise_command("capture-source-ready", "captured", quiesced=quiesced, wallets=wallets,
                             orderId="0x" + order_id, receiverPeer="0x" + receiver.device_id)
            exercise_command("resume-main", "main-miner-resumed")
            start_task_nodes(unlock=True, names=("S", "R"))
            delivery_call(source, "reconcile", order_id=order_id)
            actual_fund = transaction(source, order_id + "/fund")
            quiesced = stopped_nodes("C1Data", save=True)
            exercise_command("capture-post-fund", "captured", quiesced=quiesced, wallets=wallets,
                             orderId="0x" + order_id, fundTxHash=actual_fund)
            exercise_command("resume-main", "main-miner-resumed")
            start_task_nodes(names=("P",))
            stored_rows = delivery_rows(source, "SELECT body FROM orders WHERE id=?", (order_id,))
            require(len(stored_rows) == 1, "funded original strict Source retains its actual signed order and grants")
            stored = json.loads(stored_rows[0][0])
            grants = {grant["body"]["operation"]: grant for grant in stored["grants"] if grant["body"]["window_index"] == 0}
            ciphertext = (source.directory / "delivery/objects" / order_id / "0-ciphertext.bin").read_bytes()
            capsule = bytearray((source.directory / "delivery/objects" / order_id / "0-capsule.bin").read_bytes())
            require(len(ciphertext) > 8192 and len(capsule) == 80, "paid probe uses actual source-encrypted bytes and complete committed capsule")
            capsule[0] ^= 1
            action = {"kind": "paid_upload", "offer": {"op": "offer", "mode": "paid", "order": stored["order"],
                      "file_bytes": max_file_bytes, "intent_id": intent, "wallet_signature": stored["wallet_signature"]},
                      "ciphertext_grant": grants["put_ciphertext"], "capsule_grant": grants["put_capsule"],
                      "prefix": base64.b64encode(ciphertext[:8192]).decode(), "tampered_capsule": base64.b64encode(capsule).decode()}
            integrity = delivery_probe(options, run, source, [provider], order_id, action=action)
            durable = delivery_rows(provider, "SELECT idx,kind,committed,complete FROM objects WHERE order_id=? ORDER BY idx,kind", (order_id,))
            require(len(durable) == 4 and all(not complete for _, _, _, complete in durable)
                    and {(index, kind): committed for index, kind, committed, _ in durable} ==
                    {(0, "ciphertext"): 8192, (0, "capsule"): 0, (1, "ciphertext"): 0, (1, "capsule"): 0}
                    and not delivery_rows(provider, "SELECT id FROM transactions"),
                    "real rejected paid chunks never mark an incomplete object complete or create early payment")
            passed("paid-real-service-duplicate-reorder-offset-and-capsule-integrity", **integrity,
                   incomplete_objects=4, early_provider_transactions=0, original_order=True)
            prefix_rows, prefix_broadcasts = durable_intents(), chain_recorder.broadcasts()
            provider_exit = provider.stop(crash=True)
            require(provider_exit == -signal.SIGKILL, "the real task provider is killed before its uncommitted tail fixture")
            partial = provider.directory / "delivery/objects" / order_id / "0-ciphertext.part"
            require(provider.process is None and stat.S_ISREG(partial.lstat().st_mode)
                    and partial.read_bytes() == ciphertext[:8192], "only the actual incomplete durable provider prefix receives an uncommitted tail")
            tail = b"uncommitted-delivery-tail" * 7
            with os.fdopen(os.open(partial, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW), "wb") as output:
                output.write(tail)
                output.flush()
                os.fsync(output.fileno())
            require(delivery_rows(provider, "SELECT idx,kind,committed,complete FROM objects WHERE order_id=? ORDER BY idx,kind", (order_id,)) == durable
                    and partial.read_bytes() == ciphertext[:8192] + tail,
                    "the stopped provider fixture changes no committed database offset or complete object")
            start_task_nodes(names=("P",))
            require(cli(provider, ["status"])["state"] == "locked" and partial.stat().st_size == 8192 + len(tail)
                    and delivery_rows(provider, "SELECT committed,complete FROM objects WHERE order_id=? AND idx=0 AND kind='ciphertext'", (order_id,)) == [(8192, 0)],
                    "provider restart preserves its actual durable offset and does not claim startup truncation")
            resumed = delivery_probe(options, run, source, [provider], order_id,
                action={"kind": "paid_resume", "ciphertext_grant": grants["put_ciphertext"],
                        "chunk": base64.b64encode(ciphertext[8192:16384]).decode()})
            require(resumed["resumed_offset"] == 8192 and resumed["committed_offset"] == 16384
                    and partial.read_bytes() == ciphertext[:16384] and partial.stat().st_size == 16384
                    and delivery_rows(provider, "SELECT committed,complete FROM objects WHERE order_id=? AND idx=0 AND kind='ciphertext'", (order_id,)) == [(16384, 0)]
                    and prefix_rows == durable_intents() and chain_recorder.broadcasts() == prefix_broadcasts
                    and cli(provider, ["status"])["state"] == "locked",
                    "the first original-grant valid append discards only the uncommitted tail without another proof or transaction")
            passed("paid-provider-sigkill-resumes-committed-prefix-and-discards-uncommitted-tail", status="passed",
                   order_id=order_id, signal="SIGKILL", exit_code=provider_exit, wallet_locked=True,
                   committed_before=8192, committed_after=16384, uncommitted_tail_bytes=len(tail),
                   truncate_on_first_valid_append=True, original_ciphertext_prefix_matches=True,
                   transaction_rows_unchanged=True, proof_rows_unchanged=True, raw_broadcast_delta=0,
                   new_proofs=0, original_order=True)
            cli(provider, ["unlock"], password=passwords["P"])
            known_locked.discard("P")
            start_task_nodes(unlock=True, names=("S", "R"))
            delivery_call(source, "reconcile", order_id=order_id)
            wait_for("actual Source reconciles its original funded intent after the controlled provider outage", lambda:
                     not delivery_call(source, "get", order_id=order_id)["error"], timeout=90)
        latest = {}

        def state(node):
            value = delivery_call(node, "get", order_id=order_id)
            if value["error"]:
                raise RuntimeError("paid executor reported a failure while proving or settling: " + value["error"])
            key = (value["phase"], value["evidence"], tuple(w["state"] for w in value["windows"]))
            if latest.get(node.name) != key:
                latest[node.name] = key
                print(f"OBSERVE delivery-paid {node.name} phase={key[0]} evidence={key[1]}", flush=True)
            return value

        def offered():
            state(source)
            return next((order for order in delivery_call(receiver, "list")["orders"] if order["order_id"] == order_id), None)

        wait_for("two genuine source proofs, escrow funding and receiver offer", offered, timeout=16000)
        delivery_call(receiver, "accept", order_id=order_id)

        def completely_opened():
            value = state(receiver)
            complete = (value["evidence"] == "decryption_proven"
                        and all(w["decryption_proven"] for w in value["windows"])
                        and value["payment"]["earned"] == "29" and value["payment"]["withdrawn"] == "29")
            return value if complete else None

        final = wait_for("both genuine hold and open proofs settle strict delivery", completely_opened, timeout=32000)
        recovered = delivery_call(receiver, "read", order_id=order_id)
        data = base64.b64decode(recovered["content_base64"], validate=True)
        require(len(data) == max_file_bytes and recovered["bytes"] == max_file_bytes
                and data == original and recovered["name"] == name and recovered["sha256"] == digest,
                "strict paid delivery returns the maximum-size original file, private name and SHA-256")
        require(final["payment"]["earned"] == "29" and final["payment"]["withdrawn"] == "29"
                and final["payment"]["refunded"] == "0", "provider actually withdraws all exact earned service fees")
        passed("two-window-source-hold-open-and-original-file", order_id=order_id, windows=2,
               bytes=len(data), max_file_bytes=max_file_bytes, capacity_boundary=True, sha256=digest,
               earned="29", withdrawn="29", evidence=final["evidence"])
        register_rows = delivery_rows(receiver, "SELECT id,intent FROM transactions WHERE id=?",
                                      (f"key-{register_intent}/register",))
        require(len(register_rows) == 1, "receiving key has a unique registration transaction")
        register_id, register_intent = register_rows[0]
        calldata = bytes.fromhex(json.loads(register_intent)["calldata"].removeprefix("0x"))
        proofs = [public_proof(calldata[4:1508], receiver, 1)]
        for index in range(2):
            source_input = delivery_rows(source, "SELECT body FROM private_values WHERE id=?", (f"source-input:{order_id}:{index}",))
            require(len(source_input) == 1, "source proof input retains its exact public statement")
            journal = bytes(json.loads(source_input[0][0])["statement"])
            proofs.append(public_proof(journal, source, 2, index))
            for relation in (3, 4):
                public = journal[:32] + relation.to_bytes(32, "big") + journal[64:]
                proofs.append(public_proof(public, receiver, relation, index))
        transactions = {"registerKey": transaction(receiver, register_id), "approve": transaction(source, f"{order_id}/approve"),
                        "fund": transaction(source, f"{order_id}/fund")}
        for action in ("hold", "publish", "open", "withdraw"):
            transactions[action] = [transaction(provider, f"{order_id}/{action}-{index}") for index in range(2)]
        capsules = [{"windowIndex": index, "capsule": (provider.directory / "delivery/objects" / order_id / f"{index}-capsule.bin").read_bytes().hex()}
                    for index in range(2)]
        bundle = {"schemaVersion": 1, "escrow": chain["escrow"], "asset": chain["asset"], "orderId": "0x" + order_id,
                  "proofs": proofs, "capsules": capsules, "transactions": transactions}
        bundle_path = export / "settlement.json"
        private_json(bundle_path, bundle)
        observer = Path(configuration["settlement_observer"]).resolve(strict=True)
        observed = subprocess.run(["node", str(observer), "--settlement", str(bundle_path), "--rpc-url", chain["rpc_url"]],
                                  capture_output=True, timeout=180)
        with (run / "settlement-observer.log").open("wb") as output:
            output.write(observed.stdout[:65536])
            output.write(observed.stderr[:65536])
        require(observed.returncode == 0, "independent read-only observer verifies seven proofs, actual transactions, balances and gas on the original chain")
        require(len(observed.stdout) <= 65536, "strict public observer receipt remains bounded")
        strict_observation = json.loads(observed.stdout)
        require(strict_observation.get("status") == "PASS", "strict public observer receipt records this actual success")
        private_json(export / "settlement-observation.json", strict_observation)
        passed("seven-real-receipts-and-independent-settlement-observer", bundle=str(bundle_path), proofs=7,
               fund=transactions["fund"], withdrawals=transactions["withdraw"], read_only_observer=True)
        counts = {node.name: len(delivery_rows(node, "SELECT id FROM transactions")) for node in nodes}
        for node in (source, receiver):
            node.stop(crash=True)
            node.start()
            node.client.connect()
            node.client.authenticate()
            require(cli(node, ["status"])["state"] == "locked", "a restarted Node restores its independent wallet locked")
        recovered_again = delivery_call(receiver, "read", order_id=order_id)
        require(base64.b64decode(recovered_again["content_base64"], validate=True) == original,
                "paid file remains recoverable after a real receiver process restart")
        require_quiet("restarting completed paid Nodes creates no second nonce or transaction intent", lambda:
                      counts == {node.name: len(delivery_rows(node, "SELECT id FROM transactions")) for node in nodes})
        passed("paid-process-restart-retains-file-wallet-lock-and-transaction-identity", transaction_counts=counts)
        if chain_recorder:
            principal_service_boundaries(order_id, outsider_directory, outsider_peer)
            quiesced = stopped_nodes("MainSuccessData", save=True)
            exercise_command("verify-core", "core-verified", quiesced=quiesced, fileSha256=digest)
            incoming_cases = ("incoming-false", "incoming-frozen", "incoming-fee", "incoming-sender-tax",
                              "incoming-precision", "incoming-reentrancy", "incoming-nonreturn")
            settled_cases = ("offline-refund", "partial-refund", "held-timeout", "strict-late-open", "late-priority",
                             "hold-reorg", "capsule-reorg", "keeper", "token-false", "token-frozen", "token-fee",
                             "token-sender-tax", "token-precision", "token-nonreturn", "token-reentrancy", "node-refund-hook")
            branch_results = []
            for case in (*incoming_cases, *settled_cases):
                result = exercise_command("exercise-case", "case-complete", quiesced=last_quiesced, case=case)
                require(result["case"] == case and result["status"] == "EVM_PASS" and result["newProofs"] == 0,
                        "each finite branch actually uses the original genuine proof set without generating a replacement")
                branch_results.append(result)
            require([value["point"] for value in process_recoveries] == ["held", "key-published"]
                    and all(value["raw_broadcast_delta"] == 0 for value in process_recoveries),
                    "both actual held and key-published SIGKILL recoveries are required")
            passed("real-paid-held-and-key-published-sigkill-recovery", status="passed", points=process_recoveries,
                   new_proofs=0, original_order=True)
            exercise_command("restore-core", "core-restored", quiesced=last_quiesced)
            mixed = exercise_command("lifecycle-held", "lifecycle-held-complete", quiesced=last_quiesced)
            require(len(lifecycle_actions) == 1 and lifecycle_actions[0]["public"]["action"] == "rotate"
                    and mixed["status"] == "EVM_PASS" and mixed["oldFileSha256"] == digest,
                    "mixed held lifecycle performs one actual Rotate and recovers the old held-key original")
            restored = exercise_command("restore-core", "core-restored", quiesced=last_quiesced)
            require(restored["status"] == "PASS" and restored["restoredAuthorizedRotation"]["txHash"] == lifecycle_actions[0]["public"]["transactionHash"],
                    "canonical restoration reuses the same original Rotate transaction and retains its new key material")
            key_directory = run / "key-lifecycle"
            for action in ("revoke", "register"):
                exercise_command("resume-main", "main-miner-resumed")
                start_task_nodes()
                completed_key = genuine_key_action(action, key_directory, order_id, digest)
                lifecycle_actions.append(completed_key)
                label = "After" + action.capitalize() + "Data"
                quiesced = stopped_nodes(label, save=True)
                recorded = exercise_command("record-key", "key-recorded", quiesced=quiesced,
                                            action=action, txHash=completed_key["public"]["transactionHash"])
                require(recorded["status"] == "PASS" and recorded["serial"] == completed_key["public"]["serial"],
                        "finite lifecycle observer independently confirms the actual owner certificate serial and transaction")
            public_branches = run / "genuine-branches"
            public_branches.mkdir(mode=0o700)
            actual_exports = (repository / branch_public_index).parent
            branch_files = ["index.json", "core-restored.json", "branch-mixed-held-rotate.json",
                            *["branch-" + case + ".json" for case in (*incoming_cases, *settled_cases)],
                            *["key-" + action + ".json" for action in ("rotate", "revoke", "register")]]
            for name in branch_files:
                source_file = actual_exports / name
                require(stat.S_ISREG(source_file.lstat().st_mode) and source_file.stat().st_size <= 1024 * 1024,
                        "only the current finite session's bounded public branch receipts are exported")
                shutil.copyfile(source_file, public_branches / name)
            shutil.copyfile(public_branches / "branch-mixed-held-rotate.json", key_directory / "mixed-held.json")
            public_actions = [value["public"] for value in lifecycle_actions]
            require([value["action"] for value in public_actions] == ["rotate", "revoke", "register"]
                    and len({value["serial"] for value in public_actions}) == 3,
                    "three actual lifecycle actions have distinct observed increasing registry serials")
            key_summary = {"schemaVersion": 1, "kind": "genuine-key-lifecycle", "status": "PASS", "generatedProofs": 3,
                           **lifecycle_actions[-1]["metadata"], "operations": public_actions,
                           "oldOrderId": "0x" + order_id, "oldFileSha256": digest, "mixedHeldObservation": "mixed-held.json",
                           "finalRegistrationId": lifecycle_actions[-1]["transaction_id"], "finalActiveSerial": public_actions[-1]["serial"],
                           "finalBindingJournalHash": public_actions[-1]["journalHash"]}
            private_json(key_directory / "summary.json", key_summary)
            passed("genuine-key-lifecycle-preserves-old-key-and-enables-new-key", summary=str(key_directory / "summary.json"),
                   new_proofs=3, actions=[value["action"] for value in public_actions],
                   serials=[value["serial"] for value in public_actions], old_sha256=digest, mixed_held=True)
            passed("genuine-branches-and-canonical-restoration", status="passed", public_index=branch_public_index,
                   session_id=branch_session, helper_new_proofs=0, core_restored=True)
            require([record["kind"] for record in inspection_records] ==
                    ["receiver-key-before", "source-partial", "source-unknown", "receiver-key-after"]
                    and all(DeliveryUiControl.inspection_verified(record["checks"]) for record in inspection_records)
                    and inspection_records[0]["observed"]["receiverKey"]["keyId"] != inspection_records[3]["observed"]["receiverKey"]["keyId"],
                    "four actual rendered inspections preserve their original order and the genuine changed key")
            inspection_manifest = run / "ui-inspections.json"
            private_json(inspection_manifest, {"schemaVersion": 1, "kind": "vdp-rendered-inspections", "status": "PASS",
                         "strictOrderId": order_id, "strictIntentId": intent, "generatedProofs": 0, "inspections": inspection_records})
            passed("rendered-original-order-inspections", status="passed", manifest=str(inspection_manifest), count=4,
                   new_proofs=0, raw_broadcast_delta=0, strict_order_id=order_id)
            exercise_command("resume-main", "main-miner-resumed")
            start_task_nodes()
            ordinary_bundle = ordinary_ui_acceptance(lifecycle_actions[-1]["journal"], lifecycle_actions[-1]["transaction_id"])
            actual_proof_jobs = [(node.name, digest_value) for node in nodes
                                 for digest_value, in delivery_rows(node, "SELECT digest FROM proof_jobs")]
            require(len(actual_proof_jobs) == 13 and len({digest_value for _, digest_value in actual_proof_jobs}) == 13,
                    "final task has exactly thirteen distinct actual genuine proof reservations")
            stopped_nodes("FinalThirteenProofData", save=True)
            require(len(proof_catalog) == 13 and set(proof_catalog) == {digest_value for _, digest_value in actual_proof_jobs},
                    "all observed whole-checkpoint histories contain exactly the same thirteen final distinct genuine receipts")
            report["genuine_proofs"] = 13
            report["ordinary_bundle"] = str(ordinary_bundle)
        report["status"] = "passed"
        report["bundle"] = str(bundle_path)
    except BaseException as error:
        report["status"] = "failed"
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        passwords.clear()
        if ui_control is not None:
            ui_control.close()
        if registration_process is not None and registration_process.poll() is None:
            registration_process.terminate()
            try:
                registration_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                registration_process.kill()
                registration_process.wait(timeout=5)
        if registration_log is not None:
            registration_log.close()
        for process in key_processes:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        if branch_process is not None:
            branch_process.stdin.close()
            try:
                branch_process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                branch_process.terminate()
                try:
                    branch_process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    branch_process.kill()
                    branch_process.wait(timeout=5)
            if report["status"] == "passed" and branch_process.returncode != 0:
                report["status"] = "failed"
                report["failure"] = {"type": "GenuineExerciseExit", "message": "finite genuine exercise did not complete with its successful original session"}
        if branch_log is not None:
            branch_log.close()
        for group in {fact["group"] for fact in inherited_proof.values() if same_process(fact)}:
            if any(fact["group"] == group and same_process(fact) for fact in inherited_proof.values()):
                try:
                    os.killpg(group, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        stop_fixture_processes(principals)
        relay.close()
        if chain_recorder:
            chain_recorder.close()
        private_json(run / "summary.json", report)
        print("Paid delivery report: " + str(run / "summary.json"), flush=True)
    require(report["status"] == "passed", "actual paid acceptance and its finite exercise both complete successfully")
    return report


def e2e_fixture(options):
    program = shutil.which(str(options.binary)) if len(options.binary.parts) == 1 else str(options.binary)
    require(program is not None, "the isolated e2e Node binary exists on PATH or at its explicit path")
    binary = Path(program).resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    run = options.artifacts.resolve() / ("fixture-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    helper = run / "client.mjs"
    module = options.crypto_module.resolve()
    helper.write_text(CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    helper.chmod(0o600)
    nodes = {}
    relay = PkarrRelay() if options.fixture_kind == "delivery" else None
    rpc = DeliveryRpcSpy() if relay else None
    native_ready = False

    def details():
        return {"run": str(run), "nodes": {
            role: {"url": node.url, "peer": node.device_id, "directory": str(node.directory)}
            for role, node in nodes.items()
        }}

    def stop(*_):
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        for role in (["source", "receiver", "provider"] if relay else ["source"]):
            args = delivery_entry_args() if role == "provider" else []
            node = Node(run, role, binary, workspace, helper, init_args=args,
                        port=options.fixture_port if role == "source" else None)
            nodes[role] = node
            isolate_delivery_node(node)
            if relay:
                node.configure_network(pkarr={"publish": True, "resolve": True,
                                                 "urls": [relay.url], "publish_direct": True})
            if rpc:
                private_json(node.directory / "delivery-settings.json",
                             free_delivery_settings(rpc, options.fixture_profile, enabled=role == "provider"))
            node.start()
        print(json.dumps({"ready": details()}), flush=True)
        for line in sys.stdin:
            request = json.loads(line)
            try:
                op = request.get("op")
                if op == "restart":
                    role = request.get("role")
                    require(role in nodes, "fixture restart names its own Node")
                    node = nodes[role]
                    node.stop(crash=bool(request.get("crash")))
                    node.start()
                    result = details()
                elif op == "inspect":
                    result = {"wallet_files": sum((node.directory / "delivery-wallet.json").exists()
                                                  for node in nodes.values()),
                              "pkarr_reads": relay.reads if relay else 0,
                              "pkarr_writes": relay.writes if relay else 0,
                              "chain_rpc_calls": rpc.calls if rpc else 0}
                elif op == "native_setup":
                    require(relay is not None and not native_ready, "native setup initializes only its own fresh delivery fixture")
                    for role in ("source", "receiver"):
                        node = nodes[role]
                        node.bootstrap()
                        delivery_call(node, "configure", enabled=True, charity_enabled=True, paid_enabled=False)
                    path = Path(request["invitation_file"])
                    require(path.is_absolute() and path.resolve().is_relative_to(options.artifacts.resolve())
                            and not path.exists(), "native invitation stays in a new private task file")
                    invitation = nodes["receiver"].client.invite()
                    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as output:
                        output.write(invitation)
                    native_ready = True
                    result = details()
                elif op == "native_join":
                    require(native_ready, "native pairing requires the controlled initialized receiver")
                    client = nodes["receiver"].client

                    def revealed():
                        entries = client.call("join_status")["requests"]
                        pending = next((item for item in entries if item["name"] == "VDP native receiver"
                                        and item["kind"] == "client" and item["status"] in ("pending", "confirming")), None)
                        if pending and pending["status"] == "pending":
                            client.start_approval(pending["join_id"])
                            return None
                        return pending if pending and pending.get("joiner_nonce") else None

                    pending = wait_for("native client reveals its actual pairing nonce", revealed)
                    result = {"peer": pending["device_id"], "code": client.command("short_code", request=pending)}
                elif op == "native_approve":
                    require(native_ready, "native approval belongs to the controlled receiver")
                    client = nodes["receiver"].client
                    pending = client.pending_join(request["peer"])
                    require(pending and pending["name"] == "VDP native receiver" and pending.get("joiner_nonce")
                            and request["code"] == client.command("short_code", request=pending),
                            "native and managing device must show the exact same revealed safety code")
                    client.grant(pending, manage=True)
                    result = {"approved": True}
                elif op == "native_send":
                    require(native_ready, "native delivery is scoped to the initialized fixture")
                    intent = str(uuid.uuid4())
                    prepared = delivery_call(nodes["source"], "prepare", intent_id=intent,
                        receiver_peer=nodes["receiver"].device_id, provider_peer=nodes["provider"].device_id,
                        mode="charity", acceptance="recoverable", name=request["name"], content_base64=request["content_base64"],
                        max_service_amount="0", max_gas_amount="0")
                    delivery_call(nodes["source"], "confirm", intent_id=intent)
                    result = {"order_id": prepared["order_id"]}
                else:
                    raise ValueError("unsupported fixture operation")
                print(json.dumps({"ok": True, "result": result}), flush=True)
            except Exception as error:
                print(json.dumps({"ok": False, "error": type(error).__name__}), flush=True)
    finally:
        for node in reversed(list(nodes.values())):
            node.client.close()
            node.stop()
        if relay:
            relay.close()
        if rpc:
            rpc.close()


if __name__ == "__main__":
    os.umask(0o077)
    if "--fixture-codex" in sys.argv:
        fixture_codex(sys.argv[1:])
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--binary", type=Path, required=True)
        parser.add_argument("--artifacts", type=Path, required=True)
        parser.add_argument("--crypto-module", type=Path, default=Path(__file__).resolve().parents[1] / "crates/node/web/generated/vibeface_secure.js")
        modes = parser.add_mutually_exclusive_group()
        modes.add_argument("--web-only", action="store_true", help="check packaged Web resources without Node business or provider scenarios")
        modes.add_argument("--phase1", action="store_true", help="run focused durable Manager queue/follow-up process checks with controlled fixtures")
        modes.add_argument("--chat-agent", action="store_true", help="run direct App Server work, triage, decisions, cancellation and native observation process checks")
        modes.add_argument("--auto-route", action="store_true", help="check entry joins, origins, forwarding, recovery and uncertain writes")
        modes.add_argument("--phase4", action="store_true", help="check cross-entrance coordination, agent tasks, owner moves, revocation, checks and attachments")
        modes.add_argument("--discovery", action="store_true", help="check discovery-only connection assistance, automatic normal-relay fallback, and headless Nodes")
        modes.add_argument("--public-entry", action="store_true", help="check a public entry that forwards browsers and decisions to a member Node through its relay")
        modes.add_argument("--phase5", action="store_true", help="check write protection after a restore, alone and as a copied data directory, status, logs and diagnostics")
        modes.add_argument("--e2e-fixture", action="store_true", help=argparse.SUPPRESS)
        modes.add_argument("--delivery-charity", action="store_true", help="check free discovery, charity transfer, loopback service boundaries and durable restart")
        modes.add_argument("--delivery", action="store_true", help="check the full paid delivery flow; requires a private local deployment configuration")
        modes.add_argument("--delivery-ui-selfcheck", action="store_true", help="check only the local UI controller schema without starting Nodes, UI, chain or proofs")
        parser.add_argument("--delivery-profile", type=Path, default=Path(__file__).resolve().parents[1] / "proofs/delivery/profile.json")
        parser.add_argument("--delivery-source-root", type=Path, default=Path(__file__).resolve().parents[1])
        parser.add_argument("--delivery-probe", type=Path)
        parser.add_argument("--delivery-config", type=Path)
        parser.add_argument("--delivery-ui-control", type=Path)
        parser.add_argument("--delivery-ui-timeout-ms", type=int, default=43_200_000)
        parser.add_argument("--fixture-kind", choices=("single", "delivery"), default="single", help=argparse.SUPPRESS)
        parser.add_argument("--fixture-port", type=int, help=argparse.SUPPRESS)
        parser.add_argument("--fixture-profile", type=Path, default=Path(__file__).resolve().parents[1] / "proofs/delivery/profile.json", help=argparse.SUPPRESS)
        arguments = parser.parse_args()
        try:
            require(30_000 <= arguments.delivery_ui_timeout_ms <= 86_400_000, "UI acceptance has a bounded explicit execution deadline")
            if arguments.delivery_ui_selfcheck:
                delivery_ui_selfcheck(arguments)
            elif arguments.e2e_fixture:
                require(arguments.fixture_port is not None, "e2e fixture requires its allocated port")
                e2e_fixture(arguments)
            elif arguments.delivery:
                delivery_paid_acceptance(arguments)
            elif arguments.delivery_charity:
                delivery_charity_acceptance(arguments)
            elif arguments.discovery:
                discovery_acceptance(arguments)
            elif arguments.phase5:
                phase5_acceptance(arguments)
            elif arguments.public_entry:
                public_entry_acceptance(arguments)
            elif arguments.phase4:
                phase4_acceptance(arguments)
            elif arguments.auto_route:
                auto_route_acceptance(arguments)
            elif arguments.web_only:
                web_acceptance(arguments)
            elif arguments.chat_agent:
                chat_agent_acceptance(arguments)
            elif arguments.phase1:
                phase1_acceptance(arguments)
            else:
                acceptance(arguments)
        except Exception as error:
            print(f"FAIL {type(error).__name__}: {error}", file=sys.stderr)
            sys.exit(1)
