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
import json
import os
import re
from html.parser import HTMLParser
from pathlib import Path
import shutil
import signal
import select
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


def definitions_of(bots):
    """Listed Bots without the read-only preview of their latest message."""
    return [{key: value for key, value in bot.items() if key != "last_message"} for bot in bots]


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
  const url = new URL('/api/client', input.url.replace(/^http/, 'ws'));
  // Node's WebSocket can send the Origin a page on another client origin would send.
  const socket = new WebSocket(url, input.origin ? { headers: { Origin: input.origin } } : undefined);
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
    socket.send(JSON.stringify({ type: 'connect', target: input.device_id }));
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

    def connect(self, device_id=None, *, connection="main", check=True, origin=None):
        self.device_id = device_id or self.device_id
        return self.command("connect", url=self.url, device_id=self.device_id, connection=connection, check=check,
                            **({"origin": origin} if origin else {}))

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
    def __init__(self, run, name, binary, workspace, helper, *, init_args=None):
        self.name = name
        self.binary = binary
        self.directory = run / name
        self.port = available_port()
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
            self.configure_network(bind=f"127.0.0.1:{self.port}")
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
            configuration["providers"]["execution"] = True
            if app_server["mode"] == "spawn":
                env_path = self.directory / "provider-env.json"
                providers = json.loads(env_path.read_text()) if env_path.exists() else {"codex": {}, "cursor": {}}
                providers["codex"]["CODEX_HOME"] = str(self.directory / "home/.codex")
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
            info = wait_for(f"{self.name} HTTP listener", lambda: self.client.request("/api/info"))
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
    """Bot turns run on the fixture App Server, so `codex_bin` must be the `--fixture-codex` wrapper."""
    return {"provider": "codex_sdk", "workspace": str(workspace), "model": "controlled-fixture",
            "cursor_sdk_bin": "unused-fixture"}


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


READ_TOOLS = {"nodes", "context", "work_status", "call_lookup", "read_skill", "recall", "fetch_url", "list_bots", "read_bot"}
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
    events = json.loads(prompt[prompt.index("Events:\n") + len("Events:\n"):])
    state_path = run / "fixture-codex-state.json"
    threads = json.loads(state_path.read_text())["threads"] if state_path.exists() else {}
    for event in events:
        directive = threads.get(event["session_id"], {}).get("directive", {})
        action = {"event_id": event["event_id"], "event": event["event"], "session_id": event["session_id"],
                  "request_id": event.get("request_id"), "cwd": event["cwd"], "pid": os.getpid()}
        mode = directive.get("request_mode", "respond")
        if event.get("request_id"):
            listed = json.loads(fixture_cli(initial, "codex", "requests").stdout)["requests"]
            require(any(row["request_id"] == event["request_id"] for row in listed),
                    "codex requests lists the held App Server request")
            if mode == "respond":
                answers = [json.loads(fixture_cli(initial, "codex", "respond", event["request_id"],
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
        fixture_record(run, "triage-invocations.jsonl", {"pid": os.getpid(), "native_session_id": native})
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
              "works": [row["work_id"] for row in context["works"]] if isinstance(context["works"], list) else None,
              "bot": context["bot"].get("configured_bot"),
              "sessions": [row["session_id"] for row in context["bot"].get("sessions") or []],
              "bot_prompt_markers": [marker for marker in
                  ("bot-alpha-instructions-sentinel", "bot-beta-instructions-sentinel") if marker in initial["history"]],
              "node_url": initial["tool_env"]["VIBEFACE_NODE_URL"]}
    fixture_record(run, "manager-invocations.jsonl", record)
    schemas = set(initial["custom_tools"]) - set(context["trigger"].get("unavailable_tools", []))
    expected_tools = READ_TOOLS | WORK_TOOLS
    if kind != "agent_task":
        expected_tools |= WAKE_TOOLS
    if kind == "human":
        expected_tools |= SETTINGS_TOOLS
    require(schemas == expected_tools, "the turn receives the authoritative scoped tool catalog")

    def tool(name, arguments, expected=True, rejection="unknown", discard=False):
        environment = initial["tool_env"]
        request = urllib.request.Request(
            environment["VIBEFACE_NODE_URL"] + "/sdk.v1.SdkCustomToolCallbackService/CallCustomTool",
            json.dumps({"toolName": name, "args": arguments, "agentId": "fixture"}).encode(),
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

    if kind == "answer":
        question, answer = context["trigger"]["question"], body["text"]
        about = question["about"]
        if about.get("agent") == "vibeface":
            replies = [tool("apply_provider_config", {"question_id": question["question_id"]}) for _ in range(2)]
            require(replies[0] == replies[1] and replies[0]["status"] in ("applied", "declined"),
                    "a settings card's answer is applied once; a repeat reads it back")
        elif about.get("request_id"):
            result = json.dumps({"decision": "accept" if answer == "允许" else "decline"})
            replies = [json.loads(fixture_cli(initial, "codex", "respond", about["request_id"], result, "--node", about["node"]).stdout)
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
    if kind == "answer" and question["about"].get("agent") == "vibeface":
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
            "the coordination Agent answers the App Server request once through codex respond; a repeat returns the first answer")
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
            "the answer is one turn of the bot, whose codex respond reaches Codex once")
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
                            "turn_id": completed_turn, "cwd": threads[thread_id]["cwd"], "last_bot_message": "done"}))
        hook = {"hook_event_name": "Stop", "session_id": thread_id, "turn_id": "hook-turn-1", "cwd": str(raise_dir),
                "last_bot_message": "需要你决定下一步 needs-human"}
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
                     "last_bot_message": "dead-letter needs-human"}
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
                                                       "cwd": str(decide_dir), "last_bot_message": "choose-next needs-human"}))
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
                                                       "cwd": str(directory), "last_bot_message": message, **extra}))

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
    dropped = local_post(a, "/api/hooks", {"source": "codex_hook", "agent": "codex", "event": "turn_end",
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
                           codex_bin=str(codex) if name == "A" else sys.executable, app_server=SPAWN_APP_SERVER,
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
        for node in reversed(nodes):
            node.client.close()
            node.stop()
        stop_fixture_processes(run)
        private_json(run / "report.json", report)
        print("REPORT " + str(run / "report.json"), flush=True)


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
        a.configure(timeout_secs=600, manager=manager, codex_bin=str(codex), app_server=SPAWN_APP_SERVER,
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
        a.configure(manager=fixture_manager_settings(run, workspace), codex_bin=str(fixture_wrapper(run, "fixture-codex", "--fixture-codex")),
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
                           codex_bin=str(fixture_wrapper(root, "fixture-codex", "--fixture-codex")),
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
        require(any("p4-m2" in text for text in t2["messages"]), "the turn reads the message sent through B")
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
                "A's bot answers B's held request once with codex respond --node")
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
        require(remote["state"] == "rejected" and "outside configured roots" in remote["reason"], "B refuses an agent task outside its roots")
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
        abandoned = wait_for("A abandons the revoked device's work", lambda: (lambda row: row if row["outcome"] == "abandoned" else None)(
            works(a.client)[lost]), timeout=60)
        require(delivery(a.client, lost) is None, "the abandoned work drops its stop request")
        passed("offline-stop-and-revoke", stop="stop_timed_out", outcome=abandoned["outcome"])

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
        require(not t7["resumed"] and t7["checkpoint"] and any("p4-m7" in text for text in t7["messages"]),
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
                    codex_bin=str(fixture_wrapper(fixture, "fixture-codex", "--fixture-codex")),
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
        local_post(m, "/api/hooks", {"source": "codex_hook", "agent": "codex", "event": "turn_end", "session_id": thread_id,
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

    def reconnect(node, network, *, writable=True):
        node.client.close()
        node.client.connect(node.device_id)
        node.client.authenticate(network)
        if writable:
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
        reconnect(a, network, writable=False)
        require(a.client.call("confirm_writable")["state"] == "writable", "the test manager explicitly permits an isolated write")
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
        until(a, "read_only")
        refused = write(a.client, status=400)["error"]
        require("read-only" in refused, "alone, A refuses writes until the user confirms")
        alone = node_status(a)
        require(alone["write_state"] == "read_only" and alone["peers"][0]["reachable"] is False, "status shows read-only and the unreachable peer")
        require(a.client.call("confirm_writable")["state"] == "writable", "the user's confirmation makes A writable")
        write(a.client)
        passed("offline-node-stays-read-only-until-confirmed", refused=refused[:120])

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
        twin.client.call("confirm_writable")
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
        target.configure(manager=fixture_manager_settings(run, workspace), codex_bin=str(fixture_wrapper(run, "fixture-codex", "--fixture-codex")),
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
        arguments = parser.parse_args()
        try:
            if arguments.discovery:
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
