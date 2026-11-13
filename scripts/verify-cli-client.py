#!/usr/bin/env python3
"""Exercise the installed CLI against an owned Core and controlled Codex fixture.

No model, browser, external Node, account credentials, install or build is used.
Each invocation creates a private run below --artifacts and retains raw logs there.
The existing verify-node.py App Server fixture implements provider protocol only;
its replies do not establish real model quality or browser/native UI behavior.
"""

import argparse
import base64
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
import urllib.request
import uuid


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def private_write(path, contents):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as output:
        output.write(contents if isinstance(contents, bytes) else contents.encode())


def write_json(path, value):
    temporary = path.with_name(path.name + ".next")
    with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as output:
        json.dump(value, output, indent=2)
        output.write("\n")
    os.replace(temporary, path)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


PORTS = set()


def port():
    while True:
        with socket.socket() as tcp, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
            tcp.bind(("127.0.0.1", 0))
            value = tcp.getsockname()[1]
            if value in PORTS:
                continue
            try:
                udp.bind(("127.0.0.1", value))
            except OSError:
                continue
            PORTS.add(value)
            return value


def wait_for(description, predicate, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.15)
    raise AssertionError("timed out waiting for " + description)


def stop(process):
    if process is None:
        return
    # Every child owns a new session. Never use a name-based/global process kill.
    if process.poll() is None:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


class LostReplyProxy:
    """Forward opaque WebSocket frames, dropping replies after the staged write.

    SDK writer setup sends two Noise handshake frames, then authenticate,
    directory, subscribe and write_state. The seventh client binary frame is the
    staged input. We do not decrypt, forge or edit any frame. The test checks that
    Core stored the literal input before closing the connection, so a changed
    setup sequence fails instead of claiming an injected unknown outcome.
    """

    def __init__(self, upstream):
        self.upstream = upstream
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(1)
        self.listener.settimeout(0.2)
        self.url = "http://127.0.0.1:" + str(self.listener.getsockname()[1])
        self.forwarded = threading.Event()
        self.finished = threading.Event()
        self.lock = threading.Lock()
        self.drop = False
        self.streams = []
        self.threads = []
        self.errors = []
        self.binary_frames = 0
        self.upgrade_status = None
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    @staticmethod
    def exact(stream, length):
        data = bytearray()
        while len(data) < length:
            block = stream.recv(length - len(data))
            if not block:
                raise EOFError()
            data.extend(block)
        return bytes(data)

    @classmethod
    def header(cls, stream):
        data = bytearray()
        while not data.endswith(b"\r\n\r\n"):
            data.extend(cls.exact(stream, 1))
            require(len(data) <= 16384, "proxy HTTP header is bounded")
        return bytes(data)

    @classmethod
    def frame(cls, stream):
        header = cls.exact(stream, 2)
        length = header[1] & 127
        if length == 126:
            extra = cls.exact(stream, 2)
            length = struct.unpack("!H", extra)[0]
            header += extra
        elif length == 127:
            extra = cls.exact(stream, 8)
            length = struct.unpack("!Q", extra)[0]
            header += extra
        require(length <= 4 * 1024 * 1024, "proxy WebSocket frame is bounded")
        if header[1] & 128:
            header += cls.exact(stream, 4)
        return header[0] & 15, header + cls.exact(stream, length)

    def accept(self):
        try:
            while not self.finished.is_set():
                try:
                    downstream, _ = self.listener.accept()
                    break
                except socket.timeout:
                    continue
            else:
                return
            upstream = socket.create_connection(("127.0.0.1", self.upstream), timeout=10)
            downstream.settimeout(30)
            upstream.settimeout(30)
            self.streams.extend((downstream, upstream))
            # This fixture proxy terminates the loopback HTTP routing hop, not Noise.
            # Keep Core's configured Host/Origin boundary while forwarding every
            # encrypted WebSocket frame byte-for-byte.
            request = self.header(downstream)
            lines = request.split(b"\r\n")
            authority = ("127.0.0.1:" + str(self.upstream)).encode()
            for index, line in enumerate(lines):
                if line.lower().startswith(b"host:"):
                    lines[index] = b"Host: " + authority
                elif line.lower().startswith(b"origin:"):
                    lines[index] = b"Origin: http://" + authority
            upstream.sendall(b"\r\n".join(lines))
            response = self.header(upstream)
            self.upgrade_status = response.split(b"\r\n", 1)[0].decode("ascii", errors="replace")
            require(response.startswith(b"HTTP/1.1 101"), "proxy received a WebSocket upgrade")
            downstream.sendall(response)
            for source, target, client in ((downstream, upstream, True), (upstream, downstream, False)):
                thread = threading.Thread(target=self.pump, args=(source, target, client), daemon=True)
                self.threads.append(thread)
                thread.start()
            for thread in self.threads:
                thread.join()
        except (OSError, EOFError):
            pass
        except Exception as error:
            self.errors.append(type(error).__name__)

    def pump(self, source, target, client):
        try:
            while not self.finished.is_set():
                opcode, frame = self.frame(source)
                with self.lock:
                    if client and opcode == 2:
                        self.binary_frames += 1
                        if self.binary_frames == 7:
                            self.drop = True
                    if client or not self.drop:
                        target.sendall(frame)
                    if client and self.binary_frames == 7:
                        self.forwarded.set()
        except (OSError, EOFError):
            pass
        except Exception as error:
            self.errors.append(type(error).__name__)

    def close(self):
        self.finished.set()
        self.listener.close()
        for stream in self.streams:
            with contextlib.suppress(OSError):
                stream.shutdown(socket.SHUT_RDWR)
            stream.close()
        self.thread.join(timeout=5)
        for thread in self.threads:
            thread.join(timeout=5)
        require(not self.thread.is_alive() and not any(t.is_alive() for t in self.threads),
                "owned proxy threads stopped")


class Acceptance:
    def __init__(self, options):
        os.umask(0o077)
        options.artifacts.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.run = options.artifacts.resolve() / ("cli-process-" + uuid.uuid4().hex[:10])
        self.run.mkdir(mode=0o700)
        self.repo = Path(__file__).resolve().parents[1]
        self.binary = self.run / "vibeface"
        self.core = self.run / "vibeface-core"
        self.report = {"status": "running", "checks": [], "executables": {},
                       "scope": "real CLI/Core; controlled provider protocol",
                       "not_checked": ["real model quality or real provider authentication",
                                       "browser and native UI rendering", "cross-node delivery and paid settlement",
                                       "non-manager membership refusal and waiting member-recovery submission"]}
        self.children = []
        self.logs = []
        self.counter = 0
        self.current = "setup"
        self.proxy = None
        self.node = None
        for source, target, key in ((options.binary, self.binary, "cli"), (options.core_binary, self.core, "core")):
            source = source.resolve(strict=True)
            require(source.is_file() and os.access(source, os.X_OK), key + " is executable")
            shutil.copy2(source, target)
            self.report["executables"][key] = {"source": str(source), "sha256": sha256(target)}
        self.directory = self.run / "node"
        self.client_dir = self.run / "client"
        self.workspace = self.run / "workspace"
        self.extra_root = self.run / "proposed-root"
        for path in (self.workspace, self.extra_root, self.client_dir, self.run / "home"):
            path.mkdir(mode=0o700)
        self.passphrase = (secrets.token_urlsafe(32) + "\n").encode()
        self.recovery_password = (secrets.token_urlsafe(32) + "\n").encode()
        self.new_recovery_password = (secrets.token_urlsafe(32) + "\n").encode()
        # Keep only a minimal environment. Personal model/session/Node variables never enter children.
        home = self.run / "home"
        self.env = {"PATH": "/usr/bin:/bin", "HOME": str(home), "USERPROFILE": str(home),
                    "LANG": "C.UTF-8", "RUST_BACKTRACE": "1", "TOKIO_WORKER_THREADS": "2",
                    "CODEX_HOME": str(home / ".codex"), "CLAUDE_CONFIG_DIR": str(home / ".claude"),
                    "XDG_CONFIG_HOME": str(home / ".config"), "XDG_DATA_HOME": str(home / ".local/share"),
                    "AGENT_CLI_CREDENTIAL_STORE": "file"}
        self.peer_port, self.local_port, self.quic_port = port(), port(), port()
        self.url = f"http://127.0.0.1:{self.peer_port}"
        spec = importlib.util.spec_from_file_location("cli_acceptance_node_fixtures", self.repo / "scripts/verify-node.py")
        self.fixtures = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.fixtures)

    def check(self, name, operation):
        self.current = name
        evidence = operation() or {}
        self.report["checks"].append({"check": name, "status": "pass", **evidence})
        write_json(self.run / "summary.json", self.report)
        print("PASS " + name, flush=True)

    def launch(self, arguments, *, client=False, recovery=False, new_recovery=False, json_output=True, suffix=None, stdin_pipe=False, node_data=None):
        self.counter += 1
        stem = f"{self.counter:03d}-" + (suffix or arguments[0])
        stdout = self.run / (stem + ".stdout")
        stderr = self.run / (stem + ".stderr")
        handles = []
        descriptors = []
        command = [str(self.binary), *map(str, arguments)]
        try:
            for enabled, content, flag in ((client, self.passphrase, "--passphrase-fd"),
                                            (recovery, self.recovery_password, "--recovery-password-fd"),
                                            (new_recovery, self.new_recovery_password, "--new-recovery-password-fd")):
                if enabled:
                    read_fd, write_fd = os.pipe()
                    descriptors.extend((read_fd, write_fd))
                    os.write(write_fd, content)
                    os.close(write_fd)
                    descriptors.remove(write_fd)
                    handles.append(read_fd)
                    command.extend((flag, str(read_fd)))
            if client:
                command.extend(("--client-dir", str(self.client_dir), "--data-dir", str(node_data or self.directory)))
                if json_output:
                    command.append("--json")
            out = stdout.open("wb")
            err = stderr.open("wb")
            self.logs.extend((out, err))
            process = subprocess.Popen(command, stdout=out, stderr=err, stdin=subprocess.PIPE if stdin_pipe else subprocess.DEVNULL,
                                       pass_fds=tuple(handles), env=self.env, start_new_session=True)
            self.children.append(process)
            return process, stdout, stderr
        finally:
            for descriptor in descriptors:
                os.close(descriptor)

    def finish(self, launched, *, success=True, json_output=True, timeout=60):
        process, stdout, stderr = launched
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            stop(process)
            raise AssertionError(f"CLI timed out; inspect {stderr.name}") from None
        require((process.returncode == 0) if success else (process.returncode != 0),
                f"unexpected CLI exit {process.returncode}; inspect {stderr.name}")
        raw = stdout.read_bytes()
        if not json_output:
            return raw
        require(raw.strip(), "CLI emitted a machine document: " + stdout.name)
        try:
            return json.loads(raw)
        except (ValueError, UnicodeError):
            raise AssertionError("stdout is not exactly one JSON document: " + stdout.name) from None

    def cli(self, arguments, **kwargs):
        success = kwargs.pop("success", True)
        json_output = kwargs.get("json_output", True)
        return self.finish(self.launch(arguments, **kwargs), success=success, json_output=json_output)

    def read(self, operation, payload=None):
        args = ["client", "read", operation]
        if payload is not None:
            args.extend(("--input", json.dumps(payload)))
        return self.cli(args, client=True)

    def write(self, operation, payload, *, success=True):
        value = self.cli(["client", "write", operation, "--input", json.dumps(payload)], client=True, success=success)
        if success:
            require(value["operation"]["state"] == "accepted", operation + " has an accepted operation")
        return value["operation"]

    def input(self, message_id, text, *, intent="continue"):
        return {"conversation_id": self.conversation, "input": {
            "message_id": message_id, "target": {"kind": "bot_main", "bot_id": self.bot_id},
            "intent": intent, "sequence": 0, "text": text, "report_results": True}}

    def messages(self):
        items, cursor, seen = [], None, set()
        for _ in range(20):
            payload = {"conversation_id": self.conversation, "section": "messages", "limit": 100}
            if cursor is not None:
                payload["cursor"] = cursor
            page = self.read("conversation-history", payload)
            require(page["conversation_id"] == self.conversation and page["section"] == "messages",
                    "history retains requested scope")
            items.extend(page["items"])
            cursor = page["next_cursor"]
            require(page["complete"] == (cursor is None), "history cursor agrees with completion")
            if cursor is None:
                return items
            require(cursor not in seen, "history cursor advances")
            seen.add(cursor)
        raise AssertionError("history exceeded bounded page count")

    def exact_message(self, message_id, text):
        rows = [row for row in self.messages() if row["message_id"] == message_id]
        require(len(rows) == 1 and rows[0]["text"] == text, "Core stores one exact message ID and text")
        return rows[0]

    def setup(self):
        self.cli(["init", "--data-dir", self.directory, "--listen", f"127.0.0.1:{self.peer_port}",
                  "--local-listen", f"127.0.0.1:{self.local_port}", "--root", self.workspace, "--no-mesh"],
                 json_output=False)
        codex = self.fixtures.fixture_wrapper(self.run, "controlled-codex", "--fixture-codex")
        config_path = self.directory / "config.json"
        config = json.loads(config_path.read_text())
        config["network"]["bind"] = f"127.0.0.1:{self.quic_port}"
        require(config["network"]["mesh"]["enabled"] is False, "public mesh is disabled")
        config["triage"]["scan_codex_sessions"] = False
        config["manager"] = self.fixtures.fixture_manager_settings(self.run, self.workspace)
        config["bins"] = {"codex": str(codex)}
        config["providers"]["codex_app_server"] = self.fixtures.SPAWN_APP_SERVER
        config["providers"]["execution"] = ["codex"]
        config["timeout_secs"] = 120
        write_json(config_path, config)
        self.node = self.launch(["serve", "--data-dir", self.directory], json_output=False, suffix="core")

        def ready():
            require(self.node[0].poll() is None, "owned Node remains alive; inspect Core log")
            try:
                with urllib.request.urlopen(self.url + "/api/info", timeout=1) as response:
                    return json.load(response)
            except OSError:
                return None
        self.info = wait_for("owned Core API", ready)
        created = self.cli(["client", "create-network", "--recovery-file", self.run / "recovery.json",
                  "--name", "CLI acceptance", "--node-name", "Owned fixture"],
                 client=True, recovery=True)
        require(created["state"] == "applied" and isinstance(created["network_id"], str), "JSON network creation emits only its actual result")
        require(not (self.client_dir / "identity-operation.json").exists(), "successful creation releases its settled identity plan")
        require((self.run / "recovery.json").stat().st_mode & 0o077 == 0, "recovery file is private")
        return {"node_id": self.info["device_id"], "mesh": False}

    def recovery_review(self):
        before = self.cli(["client", "status"], client=True)
        backup_hash = sha256(self.run / "recovery.json")
        before_files = sorted(str(path.relative_to(self.client_dir)) for path in self.client_dir.rglob("*") if path.is_file())
        absent_node = self.run / "no-local-node"
        reviewed = self.cli(["client", "recover", "--review", "--recovery-file", self.run / "recovery.json",
                             "--url", self.url, "--node", self.info["device_id"]],
                            client=True, recovery=True, node_data=absent_node)
        require(reviewed["source"] == "current_state" and reviewed["required_node"] == self.info["device_id"],
                "recovery review uses the explicitly pinned remote Node")
        require(reviewed["network_id"] == before["principal"]["network_id"]
                and reviewed["new_manager"]["device_id"] == before["principal"]["device_id"],
                "review identifies the same network and prospective manager")
        require(any(member["device_id"] == self.info["device_id"] for member in reviewed["members"]),
                "verified review includes its required target")
        after = self.cli(["client", "status"], client=True)
        require(after["view"] == before["view"] and sha256(self.run / "recovery.json") == backup_hash,
                "review changes neither signed membership nor the recovery backup")
        after_files = sorted(str(path.relative_to(self.client_dir)) for path in self.client_dir.rglob("*") if path.is_file())
        require(after_files == before_files and not absent_node.exists(), "remote review creates no identity plan or local Node data")
        status = self.cli(["client", "identity-plan", "status"], client=True)
        require(status == {"plan": None}, "completed identity journal remains cleared")
        return {"source": "current_state", "remote_without_local_node": True, "identity_unchanged": True}

    def bots(self):
        self.cli(["bots", "create", "--name", "CLI acceptance bot", "--instructions", "Use the controlled fixture."],
                 client=True)
        listed = self.cli(["bots", "list", "--data-dir", self.directory])
        home = self.read("home")
        bots = [bot for bot in home["bots"] if bot["name"] == "CLI acceptance bot"]
        require(len(bots) == 1 and bots[0] in listed["bots"], "bots list and signed home agree")
        self.bot_id, self.conversation = bots[0]["bot_id"], bots[0]["conversation_id"]
        self.read("nodes")
        conversation = self.read("conversation", {"conversation_id": self.conversation})
        require(conversation["conversation"]["conversation_id"] == self.conversation, "conversation pins its scope")
        return {"bot_id": self.bot_id, "conversation_id": self.conversation}

    def malformed(self):
        # The missing profile and unreachable endpoint would fail differently after validation.
        bad = self.launch(["client", "write", "input", "--input", '{"unexpected":true}',
                           "--client-dir", self.run / "never-created", "--url", "http://127.0.0.1:1", "--json"])
        self.finish(bad, success=False, json_output=False)
        error = bad[2].read_text()
        require("invalid input payload" in error and not (self.run / "never-created").exists(),
                "typed payload is rejected before profile creation or connection")
        oversized = self.run / "oversized.json"
        private_write(oversized, b" " * (1024 * 1024 + 1))
        bad = self.launch(["client", "write", "input", "--input", "@" + str(oversized),
                           "--client-dir", self.run / "never-created", "--json"])
        self.finish(bad, success=False, json_output=False)
        require("exceeds 1 MiB" in bad[2].read_text(), "overlarge input is rejected at the boundary")

    def stable_input(self):
        text = "literal CLI input\n第二行 with unicode ✓"
        original = self.write("input", self.input("cli-stable-input", text))
        self.exact_message("cli-stable-input", text)
        replay = self.write("input", self.input("cli-stable-input", text))
        require(replay["id"] == original["id"], "same message reuses original durable operation")
        self.exact_message("cli-stable-input", text)
        failed = self.launch(["client", "write", "input", "--input", json.dumps(self.input("cli-stable-input", "changed"))], client=True)
        self.finish(failed, success=False, json_output=False)
        self.exact_message("cli-stable-input", text)
        return {"operation_id": original["id"], "message_id": "cli-stable-input", "duplicates": 0}

    def chat_attachment(self):
        content = b"CLI attachment\x00\xff\n" + bytes(range(256)) * 9
        source = self.run / "literal.bin"
        private_write(source, content)
        text = json.dumps({"label": "cli-attachment"})
        raw = self.cli(["chat", "--bot", "CLI acceptance bot", "--message", text,
                        "--message-id", "cli-attachment", "--attach", source, "--json"], client=True, json_output=False)
        documents = [json.loads(line) for line in raw.splitlines()]
        require(documents[0]["type"] == "operation" and documents[0]["operation"]["state"] == "accepted",
                "chat emits an accepted operation document")
        require(documents[-1]["type"] == "final" and documents[-1]["message_id"] == "cli-attachment"
                and documents[-1]["state"] == "accepted", "one-shot chat reports acceptance without claiming a completed response")
        row = self.exact_message("cli-attachment", text)
        digest = hashlib.sha256(content).hexdigest()
        require(any(item["hash"] == digest and item["size"] == len(content) for item in row["attachments"]),
                "history contains literal attachment digest and size")
        page = self.read("artifact-bytes", {"conversation_id": self.conversation, "blob_hash": digest, "offset": 0})
        require(base64.b64decode(page["data"], validate=True) == content, "binary artifact read preserves literal bytes")
        output = self.run / "download.bin"
        result = self.cli(["client", "files", "get", "--conversation", self.conversation,
                           "--hash", digest, "--output", output], client=True)
        require(output.read_bytes() == content and result["verified"] is True, "files get verifies and saves literal bytes")
        require(output.stat().st_mode & 0o077 == 0, "download is private")
        staged = self.cli(["client", "files", "stage", "--file", source, "--owner", "fixture-draft"], client=True)
        require(staged["attachment"] == {"hash": digest, "size": len(content), "name": source.name},
                "typed attachment staging returns exact native metadata")
        staged_text = json.dumps({"label": "cli-staged-file"})
        staged_input = self.input("cli-staged-file", staged_text)
        staged_input["input"]["attachments"] = [staged["attachment"]]
        self.write("input", staged_input)
        staged_row = self.exact_message("cli-staged-file", staged_text)
        require(staged_row["attachments"] == [staged["attachment"]], "typed input uploads the staged attachment")
        released = self.cli(["client", "files", "release", "--owner", staged["owner"]], client=True)
        require(released == {"owner": "fixture-draft", "released": True}, "draft owner releases without changing the message")
        wait_for("controlled reply in actual history", lambda: any(row.get("text") == "cli-attachment:complete" for row in self.messages()))
        return {"sha256": digest, "bytes": len(content), "controlled_provider_reply": "cli-attachment:complete"}

    def wait_reply(self):
        text = json.dumps({"label": "cli-wait"})
        raw = self.cli(["chat", "--bot", self.bot_id, "--message", text,
                        "--message-id", "cli-wait", "--wait", "--timeout", "20", "--json"],
                       client=True, json_output=False)
        documents = [json.loads(line) for line in raw.splitlines()]
        final = documents[-1]
        require(final["type"] == "final" and final["message_id"] == "cli-wait"
                and final["state"] == "completed" and final["reply"]["text"] == "cli-wait:complete"
                and final["timed_out"] is False, "chat waits for its exact successful reply")
        self.exact_message("cli-wait", text)
        return {"message_id": "cli-wait", "reply": "cli-wait:complete"}

    def interactive_chat(self):
        launched = self.launch(["chat", "--bot", self.bot_id, "--interactive", "--timeout", "20", "--json"],
                               client=True, json_output=False, stdin_pipe=True, suffix="interactive")
        process, stdout, _ = launched
        process.stdin.write((json.dumps({"label": "cli-interactive"}) + "\n").encode())
        process.stdin.flush()

        def replied():
            require(process.poll() is None, "interactive chat remains alive while waiting")
            for line in stdout.read_text().splitlines():
                try:
                    document = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if (document.get("type") == "final" and document.get("state") == "completed"
                        and document.get("reply", {}).get("text") == "cli-interactive:complete"):
                    return document["message_id"]
            return None

        message_id = wait_for("interactive exact reply", replied)
        process.stdin.write(b"/questions\n/history\n/quit\n")
        process.stdin.flush()
        process.stdin.close()
        raw = self.finish(launched, json_output=False)
        documents = [json.loads(line) for line in raw.splitlines()]
        require(any(document.get("type") == "history" for document in documents)
                and any(document.get("type") == "questions" for document in documents),
                "interactive slash commands preserve structured output")
        self.exact_message(message_id, json.dumps({"label": "cli-interactive"}))
        return {"message_id": message_id, "reply": "cli-interactive:complete", "detached": True}

    def private_token_output(self):
        output = self.run / "decision-token.json"
        document = self.cli(["client", "write", "create-decision-token", "--input", '{"label":"CLI fixture"}',
                             "--secret-output", output], client=True)
        require(document["secret_saved"] is True and output.stat().st_mode & 0o077 == 0,
                "one-time token is saved only to a private explicit file")
        private = json.loads(output.read_text())
        require(isinstance(private["token"], str) and private["token"] not in json.dumps(document),
                "normal result does not expose the decision token")
        self.write("revoke-decision-token", {"token_id": private["token_id"]})
        return {"private_output": True, "token_revoked": True}

    def recovery_rotation(self):
        before = self.cli(["client", "status"], client=True)
        original_hash = sha256(self.run / "recovery.json")
        replacement = self.run / "rotated-recovery.json"
        result = self.cli(["client", "rotate-recovery", "--recovery-file", self.run / "recovery.json",
                           "--new-recovery-file", replacement], client=True, recovery=True, new_recovery=True)
        require(result["state"] == "applied" and replacement.stat().st_mode & 0o077 == 0,
                "rotation saves a private replacement and reports actual application")
        after = self.cli(["client", "status"], client=True)
        require(after["view"]["members"] == before["view"]["members"]
                and after["view"]["recovery_key"] != before["view"]["recovery_key"],
                "rotation changes the recovery key while preserving exact membership")
        require(sha256(self.run / "recovery.json") == original_hash
                and not (self.client_dir / "identity-operation.json").exists(),
                "rotation keeps the original backup and clears only its settled journal")
        return {"state": "applied", "members_preserved": True, "different_password_channels": True}

    def watch_writer(self):
        watch = self.launch(["client", "watch", "--conversation", self.conversation], client=True, suffix="watch")
        def snapshot():
            require(watch[0].poll() is None, "watch stays connected")
            return watch[1].stat().st_size > 0
        wait_for("watch snapshot", snapshot)
        text = "Separate writer while watch owns only a reader"
        self.write("input", self.input("cli-during-watch", text))
        self.exact_message("cli-during-watch", text)
        def pushed():
            complete = watch[1].read_text().rsplit("\n", 1)[0]
            for line in complete.splitlines():
                document = json.loads(line)
                payload = document.get("payload", {})
                if document.get("type") == "event" and any(
                    row.get("message_id") == "cli-during-watch" and row.get("text") == text
                    for row in payload.get("messages", [])
                ):
                    return True
            return False
        wait_for("watch receives live event", pushed)
        require(watch[0].poll() is None, "watch and separate writer coexist")
        stop(watch[0])
        documents = [json.loads(line) for line in watch[1].read_text().splitlines()]
        require(documents[0]["type"] == "snapshot", "watch begins with a machine snapshot")
        return {"documents": len(documents)}

    def settings_workspace(self):
        original = self.read("node-settings")
        self.write("node-settings", {"roots": [str(self.workspace), str(self.extra_root)], "manager": original["settings"]["manager"]})
        actual = self.read("node-settings")
        require(set(actual["settings"]["roots"]) == {str(self.workspace), str(self.extra_root)}, "typed settings persist exact roots")
        task_id = "cli-task-" + uuid.uuid4().hex[:12]
        self.write("workspace", {"command": {"kind": "create_task", "task_id": task_id,
            "owner_bot": self.bot_id, "title": "CLI task", "request": "Hold for explicit input",
            "expected_result": "Literal workspace state", "source_events": [], "dependencies": [], "reply_only": False}})
        workspace = self.read("workspace")
        require(any(row.get("task_id") == task_id and row.get("title") == "CLI task"
                    for row in workspace["tasks"]), "workspace read returns created task")
        self.write("delivery", {"op": "configure", "enabled": True, "charity_enabled": True, "paid_enabled": False})
        delivery = self.read("delivery", {"op": "status"})
        require(delivery["enabled"] is True and delivery["paid_enabled"] is False, "delivery setting reads back without a transfer")
        self.write("delivery", {"op": "configure", "enabled": False, "charity_enabled": True, "paid_enabled": False})
        return {"task_id": task_id, "delivery": "configuration only; no transfer or payment"}

    def questions_and_stop(self):
        # A real scoped Agent tool creates the card. No database or event injection.
        proposed = self.run / "question-root"
        proposed.mkdir(mode=0o700)
        text = json.dumps({"label": "cli-question", "tools": [{"name": "propose_provider_config",
                          "arguments": {"roots": [str(proposed)]}}]})
        self.write("input", self.input("cli-question", text))
        def question():
            return next((q for q in self.read("questions")["questions"]
                         if q["conversation_id"] == self.conversation), None)
        card = wait_for("controlled settings question", question)
        question_id = card["question_id"]
        self.write("answer", {"conversation_id": self.conversation, "question_id": question_id, "text": "不允许"})
        wait_for("signed answer in history", lambda: any(row.get("answers") == question_id and row["text"] == "不允许"
                                                       for row in self.messages()))
        require(str(proposed) not in self.read("node-settings")["settings"]["roots"], "declined settings card does not add its root")
        gate = self.run / "never-release-stop"
        label = "cli-stop"
        text = json.dumps({"label": label, "gate": str(gate)})
        timed = self.launch(["chat", "--bot", self.bot_id, "--message", text,
                            "--message-id", label, "--wait", "--timeout", "1", "--json"],
                           client=True, json_output=False, suffix="chat-timeout")
        raw = self.finish(timed, success=False, json_output=False)
        documents = [json.loads(line) for line in raw.splitlines()]
        final = documents[-1]
        require(final["type"] == "final" and final["message_id"] == label and final["timed_out"] is True
                and final["state"] != "completed", "chat wait timeout preserves original ID and does not claim completion")
        self.exact_message(label, text)
        wait_for("controlled reply started", lambda: any(row.get("label") == label
                 for row in self.fixtures.fixture_records(self.run)))
        self.write("stop-reply", {"conversation_id": self.conversation, "trigger": "message:" + label})
        def stopped():
            state = self.read("conversation", {"conversation_id": self.conversation})
            return any(row.get("trigger") == "message:" + label and row["state"] in ("cancelled", "interrupted")
                       for row in state["scheduler"]["jobs"])
        wait_for("scheduler records interrupted reply", stopped)
        return {"question_id": question_id, "stop_trigger": "message:" + label, "chat_timeout_exit": "unsuccessful"}

    def lost_reply(self):
        self.proxy = LostReplyProxy(self.peer_port)
        text = "Core accepted this exact input while the caller lost its reply"
        pending = self.launch(["client", "write", "input", "--input", json.dumps(self.input("cli-unknown", text)),
                               "--url", self.proxy.url], client=True, suffix="lost-reply")
        def forwarded():
            require(not self.proxy.errors, "controlled proxy handshake failed: " + str(self.proxy.upgrade_status))
            require(pending[0].poll() is None or self.proxy.forwarded.is_set(), "caller exited before the controlled write was forwarded")
            return self.proxy.forwarded.is_set()
        wait_for("staged write forwarded through controlled proxy", forwarded)
        wait_for("Core history confirms accepted input before disconnect", lambda: next(
            (row for row in self.messages() if row["message_id"] == "cli-unknown" and row["text"] == text), None))
        self.proxy.close()
        require(not self.proxy.errors, "controlled proxy has no framing failure")
        document = self.finish(pending, success=False)
        operation = document["operation"]
        require(operation["state"] == "unknown", "lost response remains unknown with unsuccessful exit")
        original_id = operation["id"]
        outstanding = self.cli(["client", "pending"], client=True)
        require(any(row["id"] == original_id and row["state"] == "unknown" for row in outstanding["operations"]),
                "pending retains original unknown operation")
        result = self.cli(["client", "reconcile", original_id], client=True)
        require(result["operation"]["id"] == original_id and result["operation"]["state"] == "accepted",
                "reconcile reads original Core acceptance")
        self.exact_message("cli-unknown", text)
        return {"operation_id": original_id, "initial_state": "unknown", "reconciled_state": "accepted", "duplicates": 0}

    def capabilities(self):
        catalog = self.cli(["client", "capabilities", "--json"])
        operations = catalog["operations"]
        commands = [entry["command"] for entry in operations]
        require(len(commands) == len(set(commands)) and len(commands) > 50, "capability commands are unique and complete enough for the client")
        require("client write input" in commands and "client read conversation-history" in commands,
                "runtime catalog names exercised operations")
        return {"operations": len(commands)}

    def execute(self):
        try:
            for name, method in (("private real Core and CLI network", self.setup),
                                 ("read-only remote recovery review and settled identity cleanup", self.recovery_review),
                                 ("typed invalid and oversized input rejected before connection", self.malformed),
                                 ("bots and signed home, nodes and conversation reads", self.bots),
                                 ("exact input, stable ID replay and changed-content refusal", self.stable_input),
                                 ("chat, literal attachment upload, read and verified download", self.chat_attachment),
                                 ("one-shot chat waits for its exact successful reply", self.wait_reply),
                                 ("interactive chat, slash commands and detach", self.interactive_chat),
                                 ("explicit private decision-token output", self.private_token_output),
                                 ("read-only watch coexists with separate durable writer", self.watch_writer),
                                 ("typed settings, workspace and delivery configuration", self.settings_workspace),
                                 ("signed question answer and stop controlled reply", self.questions_and_stop),
                                 ("member-preserving recovery rotation and journal cleanup", self.recovery_rotation),
                                 ("lost reply stays unknown and reconciles original ID", self.lost_reply),
                                 ("machine capability catalog", self.capabilities)):
                self.check(name, method)
            self.report["status"] = "pass"
        except BaseException as error:
            self.report["status"] = "fail"
            # Exceptions from product data never enter the shareable summary.
            self.report["checks"].append({"check": self.current, "status": "fail", "error_type": type(error).__name__})
            private_write(self.run / "failure.txt", f"{type(error).__name__}: {error}\n")
        finally:
            cleanup_errors = []
            if self.proxy:
                try:
                    self.proxy.close()
                except Exception as error:
                    cleanup_errors.append(type(error).__name__)
            for process in reversed(self.children):
                try:
                    stop(process)
                except Exception as error:
                    cleanup_errors.append(type(error).__name__)
            # Core's App Server can own a separate process group. Match both its
            # exact task root and fixture mode before cleaning any recorded PID.
            fixture_pids = set()
            try:
                fixture_pids = {row["pid"] for row in self.fixtures.fixture_records(self.run, "codex-server.jsonl")}
                for pid in fixture_pids:
                    cmdline = Path(f"/proc/{pid}/cmdline")
                    if not cmdline.exists():
                        continue
                    arguments = cmdline.read_bytes().split(b"\0")
                    if os.fsencode(self.run) in arguments and b"--fixture-codex" in arguments:
                        with contextlib.suppress(ProcessLookupError):
                            os.kill(pid, signal.SIGKILL)
                def fixtures_stopped():
                    for pid in fixture_pids:
                        stat = Path(f"/proc/{pid}/stat")
                        if stat.exists() and stat.read_text().rsplit(")", 1)[-1].split()[0] != "Z":
                            return False
                    return True
                wait_for("owned fixture processes stopped", fixtures_stopped, timeout=5)
            except Exception as error:
                cleanup_errors.append(type(error).__name__)
            for log in self.logs:
                log.close()
            self.report["cleanup"] = {"owned_processes_reaped": all(p.poll() is not None for p in self.children),
                                      "errors": cleanup_errors}
            if cleanup_errors or not self.report["cleanup"]["owned_processes_reaped"]:
                self.report["status"] = "fail"
            write_json(self.run / "summary.json", self.report)
        print(json.dumps({"status": self.report["status"], "summary": str(self.run / "summary.json")}))
        return 0 if self.report["status"] == "pass" else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--core-binary", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    return Acceptance(parser.parse_args()).execute()


if __name__ == "__main__":
    sys.exit(main())
