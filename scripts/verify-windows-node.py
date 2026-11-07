#!/usr/bin/env python3
"""Runs a real Vibeface Node on Windows and checks what only a running process can show.

Everything here uses the programs in one package directory, a data directory whose path holds
spaces and characters outside ASCII, and a controlled fake provider. No real AI provider is
called, no model key is read, and nothing outside the scratch directory is written.

    python scripts/verify-windows-node.py --package target\\x86_64-pc-windows-msvc\\debug
"""

import argparse
import base64
import ctypes
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

DATA_EPOCH = 1
MAINTENANCE_PROTOCOL = 1
WORKSPACE_NAME = "vibeface windows 验证"
NODE_NAME = "节点 one"
STILL_RUNNING = 259


class IncompleteAcceptance(AssertionError):
    """The check preserved existing host state but could not cover a required scenario."""


class FixtureProcessCheckError(AssertionError):
    """A safe, categorized process identity failure without IDs or filesystem paths."""

    def __init__(self, role, reason):
        self.role = role
        self.reason = reason
        super().__init__(f"fake provider {role} identity check failed: {reason}")


def private_pipe_diagnostics(device_id, endpoint):
    """Return pipe namespace facts without retaining or emitting endpoint names."""
    prefix = f"vibeface-{device_id[:32]}-maintenance.sock-"
    pipe_root = "\\\\.\\pipe\\"
    named_pipe = isinstance(endpoint, str) and endpoint.startswith(pipe_root)
    leaf = endpoint[len(pipe_root):] if named_pipe else ""
    scoped = bool(leaf and leaf.startswith(prefix))
    suffix_valid = bool(re.fullmatch(re.escape(prefix) + r"[0-9a-f]{16}", leaf))
    result = {
        "reported_endpoint_is_named_pipe": named_pipe,
        "reported_endpoint_matches_device_scope": scoped,
        "reported_endpoint_suffix_shape_valid": suffix_valid,
        "device_scope_pipe_count": None,
        "reported_endpoint_present_in_namespace": None,
        "reported_endpoint_wait_state": "not_checked",
    }
    if os.name != "nt" or not named_pipe:
        return result

    class FILETIME(ctypes.Structure):
        _fields_ = [("low", ctypes.c_ulong), ("high", ctypes.c_ulong)]

    class WIN32_FIND_DATAW(ctypes.Structure):
        _fields_ = [
            ("attributes", ctypes.c_ulong),
            ("created", FILETIME),
            ("accessed", FILETIME),
            ("written", FILETIME),
            ("size_high", ctypes.c_ulong),
            ("size_low", ctypes.c_ulong),
            ("reserved0", ctypes.c_ulong),
            ("reserved1", ctypes.c_ulong),
            ("name", ctypes.c_wchar * 260),
            ("alternate_name", ctypes.c_wchar * 14),
        ]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.FindFirstFileW.argtypes = [ctypes.c_wchar_p, ctypes.POINTER(WIN32_FIND_DATAW)]
    kernel.FindFirstFileW.restype = ctypes.c_void_p
    kernel.FindNextFileW.argtypes = [ctypes.c_void_p, ctypes.POINTER(WIN32_FIND_DATAW)]
    kernel.FindNextFileW.restype = ctypes.c_int
    kernel.FindClose.argtypes = [ctypes.c_void_p]
    kernel.FindClose.restype = ctypes.c_int
    data = WIN32_FIND_DATAW()
    handle = kernel.FindFirstFileW(pipe_root + "*", ctypes.byref(data))
    if handle == ctypes.c_void_p(-1).value:
        if ctypes.get_last_error() in (2, 3, 18):
            result["device_scope_pipe_count"] = 0
            result["reported_endpoint_present_in_namespace"] = False
    else:
        count = 0
        exact = False
        try:
            while True:
                candidate = data.name
                if candidate.casefold().startswith(prefix.casefold()):
                    count += 1
                    exact |= candidate.casefold() == leaf.casefold()
                if not kernel.FindNextFileW(handle, ctypes.byref(data)):
                    if ctypes.get_last_error() == 18:
                        result["device_scope_pipe_count"] = count
                        result["reported_endpoint_present_in_namespace"] = exact
                    break
        finally:
            kernel.FindClose(handle)

    kernel.WaitNamedPipeW.argtypes = [ctypes.c_wchar_p, ctypes.c_ulong]
    kernel.WaitNamedPipeW.restype = ctypes.c_int
    if kernel.WaitNamedPipeW(endpoint, 1):
        result["reported_endpoint_wait_state"] = "available"
    else:
        result["reported_endpoint_wait_state"] = {
            2: "not_found", 3: "not_found", 121: "busy",
        }.get(ctypes.get_last_error(), "other_error")
    return result


def status_observation(result, node):
    """Summarize process and endpoint identity without exposing IDs or names."""
    try:
        payload = json.loads(result.stdout)
    except (TypeError, ValueError):
        payload = None
    if not isinstance(payload, dict):
        payload = {}
    process_alive = node.process is not None and node.process.poll() is None
    return {
        "core_process_alive": process_alive,
        "http_device_matches_initialized": node.http_device_id == node.device_id,
        "status_device_matches_initialized": payload.get("device_id") == node.device_id,
        "status_value": payload.get("status") if payload.get("status") in
            ("running", "stopped", "unavailable") else "unknown",
        "pipe": private_pipe_diagnostics(node.device_id, payload.get("maintenance_socket")),
    }


class StatusCommandFailure(AssertionError):
    """A nonzero CLI status result with bounded, path- and secret-redacted evidence."""

    def __init__(self, result, node, diagnose_runtime=False):
        self.returncode = result.returncode
        self.category, self.stderr = safe_status_stderr(result.stderr, node)
        self.node_status = 'unknown'
        self.probe_error = ''
        self.runtime_diagnostics = None
        try:
            status = json.loads(result.stdout)
        except ValueError:
            status = None
        if isinstance(status, dict):
            if status.get('status') in ('running', 'stopped', 'unavailable'):
                self.node_status = status['status']
            if isinstance(status.get('error'), str):
                _, self.probe_error = safe_status_stderr(status['error'].encode('utf-8', errors='replace'), node)
        if diagnose_runtime:
            try:
                self.runtime_diagnostics = status_observation(result, node)
            except Exception:
                self.runtime_diagnostics = {"diagnostic_state": "unavailable"}
            try:
                self.runtime_diagnostics["one_immediate_followup"] = node.followup_status_observation()
            except Exception:
                self.runtime_diagnostics["one_immediate_followup"] = {"command_state": "unavailable"}
        super().__init__("status inspects the initialized node")


def safe_status_stderr(stderr, node):
    text = stderr.decode(errors="replace")
    paths = (node.directory, node.home, node.home / ".codex", node.cli, node.core,
             node.directory.parent)
    spellings = {
        spelling
        for path in paths
        for spelling in (str(path), str(path).replace("/", "\\"), str(path).replace("\\", "/"))
        if spelling
    }
    for spelling in sorted(spellings, key=len, reverse=True):
        text = text.replace(spelling, "[path]")
    device_id = getattr(node, "device_id", "")
    if device_id:
        text = text.replace(device_id, "[device id]")
        pipe_prefix = re.escape(
            rf"\\.\pipe\vibeface-{device_id[:32]}-maintenance.sock-"
        )
        text = re.sub(pipe_prefix + r"[0-9a-f]{16}", "[private pipe]", text, flags=re.IGNORECASE)
    text = re.sub(
        r"(?i)\b(?:authorization\s*:\s*)?(?:bearer|basic)\s+[^\s,;]+",
        "[redacted authorization]", text,
    )
    text = re.sub(
        r"(?i)\b(auth[_ -]?token|api[_ -]?key|access[_ -]?token|refresh[_ -]?token|"
        r"token|credential|auth(?:orization)?|password|secret)\b"
        r"(\s*[:=]\s*|\s+)[^\s,;]+",
        r"\1\2[redacted]", text,
    )
    text = re.sub(r"(?i)https?://[^/\s:@]+:[^/\s@]+@", "https://[redacted]@", text)
    text = "".join(character for character in text if character in "\n\t" or character.isprintable())
    text = text.strip()[:512]
    lowered = text.lower()
    if not text:
        category = "empty_stderr"
    elif "core control process failed" in lowered:
        category = "core_control_process_failed"
    elif "start core control process" in lowered:
        category = "core_control_launch_failed"
    elif "invalid core control response" in lowered:
        category = "invalid_core_control_response"
    else:
        category = "cli_status_failed"
    return category, text


def require(condition, description):
    if not condition:
        raise AssertionError(description)


def wait_for(description, predicate, timeout=45):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, ValueError, AssertionError, urllib.error.URLError) as error:
            last = error
        time.sleep(0.2)
    suffix = f" ({type(last).__name__}: {last})" if last else ""
    raise AssertionError(f"timed out waiting for {description}{suffix}")


def run(arguments, timeout=60, check=True, cwd=None, env=None):
    result = subprocess.run(
        [str(part) for part in arguments],
        capture_output=True, timeout=timeout, cwd=cwd, env=env,
    )
    if check and result.returncode != 0:
        raise AssertionError(
            f"{arguments[0]} failed ({result.returncode}): "
            f"{result.stderr.decode(errors='replace')[-600:]}"
        )
    return result


def free_port():
    with socket.socket() as stream:
        stream.bind(("127.0.0.1", 0))
        return stream.getsockname()[1]


def process_is_running(pid):
    """Whether a process this account may query is still running."""
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
    kernel.OpenProcess.restype = ctypes.c_void_p
    kernel.GetExitCodeProcess.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong)]
    kernel.GetExitCodeProcess.restype = ctypes.c_int
    kernel.CloseHandle.argtypes = [ctypes.c_void_p]
    kernel.CloseHandle.restype = ctypes.c_int
    handle = kernel.OpenProcess(0x1000, False, int(pid))
    if not handle:
        return False
    try:
        code = ctypes.c_ulong(0)
        if not kernel.GetExitCodeProcess(ctypes.c_void_p(handle), ctypes.byref(code)):
            return False
        return code.value == STILL_RUNNING
    finally:
        kernel.CloseHandle(ctypes.c_void_p(handle))


def open_fixture_process(pid, created, expected_image):
    """Open and identity-check a fixture process, retaining its handle to pin its PID."""
    class FILETIME(ctypes.Structure):
        _fields_ = [("low", ctypes.c_ulong), ("high", ctypes.c_ulong)]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
    kernel.OpenProcess.restype = ctypes.c_void_p
    kernel.GetExitCodeProcess.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong)]
    kernel.GetProcessTimes.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(FILETIME), ctypes.POINTER(FILETIME),
        ctypes.POINTER(FILETIME), ctypes.POINTER(FILETIME),
    ]
    kernel.QueryFullProcessImageNameW.argtypes = [
        ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_wchar),
        ctypes.POINTER(ctypes.c_ulong),
    ]
    kernel.CloseHandle.argtypes = [ctypes.c_void_p]
    handle = kernel.OpenProcess(0x1000, False, int(pid))
    if not handle:
        return None, "open_failed"
    try:
        code = ctypes.c_ulong()
        times = [FILETIME() for _ in range(4)]
        if not kernel.GetExitCodeProcess(handle, ctypes.byref(code)):
            kernel.CloseHandle(handle)
            return None, "exit_code_query_failed"
        if code.value != STILL_RUNNING:
            kernel.CloseHandle(handle)
            return None, "process_not_running"
        if not kernel.GetProcessTimes(handle, *(ctypes.byref(value) for value in times)):
            kernel.CloseHandle(handle)
            return None, "creation_time_query_failed"
        actual_created = (times[0].high << 32) | times[0].low
        if actual_created != created:
            kernel.CloseHandle(handle)
            return None, "creation_time_mismatch"
        image = ctypes.create_unicode_buffer(32768)
        length = ctypes.c_ulong(len(image))
        if not kernel.QueryFullProcessImageNameW(handle, 0, image, ctypes.byref(length)):
            kernel.CloseHandle(handle)
            return None, "image_query_failed"
        try:
            same_image = os.path.samefile(image.value, expected_image)
        except OSError:
            same_image = False
        if not same_image:
            kernel.CloseHandle(handle)
            return None, "image_mismatch"
        return handle, None
    except BaseException:
        kernel.CloseHandle(handle)
        raise


def fixture_process_is_running(pid, created, expected_image):
    handle, _ = open_fixture_process(pid, created, expected_image)
    if handle is None:
        return False
    ctypes.WinDLL("kernel32", use_last_error=True).CloseHandle(ctypes.c_void_p(handle))
    return True


def owner_only(path):
    """Read trustees as SIDs so account names and the Windows locale cannot change the check."""
    script = r"""
$ErrorActionPreference = 'Stop'
$sidType = [System.Security.Principal.SecurityIdentifier]
$current = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$acl = Get-Acl -LiteralPath $env:VIBEFACE_ACL_FIXTURE
$rules = @($acl.GetAccessRules($true, $true, $sidType))
$allow = @($rules | Where-Object {
    $_.AccessControlType -eq [System.Security.AccessControl.AccessControlType]::Allow
} | ForEach-Object { $_.IdentityReference.Value })
@{ owner = $acl.GetOwner($sidType).Value; current = $current; allow = $allow;
   protected = $acl.AreAccessRulesProtected;
   inherited = @($rules | Where-Object { $_.IsInherited }).Count } | ConvertTo-Json -Compress
"""
    encoded = base64.b64encode(script.encode('utf-16le')).decode('ascii')
    result = run(['powershell.exe', '-NoProfile', '-NonInteractive', '-EncodedCommand', encoded],
                 env={**os.environ, 'VIBEFACE_ACL_FIXTURE': str(path)})
    acl = json.loads(result.stdout.decode('utf-8-sig'))
    require(acl['owner'] == acl['current'], 'the private path is owned by this Windows SID')
    require(acl['allow'] and all(sid == acl['current'] for sid in acl['allow']),
            'the private path grants access only to this Windows SID')
    require(acl['protected'] and acl['inherited'] == 0,
            'the private path has a protected DACL with no inherited rules')
    return True


def maintenance_probe(pipe_name):
    """One request over this account's private maintenance endpoint."""
    request = json.dumps({"protocol": MAINTENANCE_PROTOCOL, "command": "probe"}).encode() + b"\n"
    with open(pipe_name, "r+b", buffering=0) as pipe:
        pipe.write(request)
        response = b""
        while not response.endswith(b"\n"):
            part = pipe.read(1)
            require(part and len(response) < 65536, "bounded maintenance response")
            response += part
    value = json.loads(response)
    require(value["protocol"] == MAINTENANCE_PROTOCOL, "maintenance protocol matches")
    return value


class FakeProvider:
    """A provider program that starts a child and waits, so a stop must clear both."""

    SOURCE = '''\
import os, subprocess, sys, time
import ctypes, json
from pathlib import Path
marks = Path(os.environ["VIBEFACE_FAKE_PROVIDER_MARKS"])
marks.mkdir(parents=True, exist_ok=True)
class FILETIME(ctypes.Structure):
    _fields_ = [("low", ctypes.c_ulong), ("high", ctypes.c_ulong)]
kernel = ctypes.WinDLL("kernel32", use_last_error=True)
kernel.GetCurrentProcess.restype = ctypes.c_void_p
kernel.GetProcessTimes.argtypes = [ctypes.c_void_p, ctypes.POINTER(FILETIME), ctypes.POINTER(FILETIME), ctypes.POINTER(FILETIME), ctypes.POINTER(FILETIME)]
def mark(name):
    created, exited, kernel_time, user_time = FILETIME(), FILETIME(), FILETIME(), FILETIME()
    if not kernel.GetProcessTimes(kernel.GetCurrentProcess(), ctypes.byref(created), ctypes.byref(exited), ctypes.byref(kernel_time), ctypes.byref(user_time)):
        raise OSError(ctypes.get_last_error(), "GetProcessTimes")
    value = (created.high << 32) | created.low
    (marks / name).write_text(json.dumps({"pid": os.getpid(), "created": value}))
if os.environ.get("VIBEFACE_FAKE_PROVIDER_CHILD"):
    mark("child.pid")
    time.sleep(900)
    sys.exit(0)
if len(sys.argv) != 4 or sys.argv[1:3] != ["app-server", "--listen"] or not sys.argv[3].startswith("unix://"):
    raise SystemExit("The fixture only accepts the documented Unix socket transport")
mark("provider.pid")
(marks / "argv.json").write_text(json.dumps(sys.argv[1:]))
environment = {**os.environ, "VIBEFACE_FAKE_PROVIDER_CHILD": "1"}
subprocess.Popen([sys.executable, __file__], env=environment,
                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                 stderr=subprocess.DEVNULL)
time.sleep(900)
'''

    def __init__(self, directory):
        self.directory = directory
        self.marks = directory / "marks"
        directory.mkdir(parents=True, exist_ok=True)
        self.source = directory / "fake_provider.py"
        self.source.write_text(self.SOURCE, encoding="utf-8")
        self.program = directory / "codex.cmd"
        self.program.write_text(
            '@echo off\r\n"%VIBEFACE_FAKE_PROVIDER_PYTHON%" "%~dp0fake_provider.py" %*\r\n',
            encoding="ascii",
        )

    def environment(self, base):
        return {**base, "VIBEFACE_FAKE_PROVIDER_MARKS": str(self.marks),
                "VIBEFACE_FAKE_PROVIDER_PYTHON": sys.executable}

    def pids(self):
        found = {}
        for name in ("provider.pid", "child.pid"):
            path = self.marks / name
            if path.exists():
                try:
                    value = json.loads(path.read_text(encoding="utf-8"))
                except (OSError, ValueError):
                    continue
                if isinstance(value.get("pid"), int) and isinstance(value.get("created"), int):
                    found[name] = (value["pid"], value["created"])
        return found

    def stop_children(self):
        """Stop fixture processes only while a verified process handle pins their PIDs."""
        failed = False
        for pid, created in self.pids().values():
            handle, _ = open_fixture_process(pid, created, sys.executable)
            if not handle:
                continue
            try:
                subprocess.run(
                    [str(Path(os.environ["SystemRoot"]) / "System32/taskkill.exe"),
                     "/PID", str(pid), "/T", "/F"],
                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL, timeout=30, check=False,
                )
                wait_for("an owned fake-provider process to stop",
                         lambda: not process_is_running(pid), timeout=15)
            except Exception:
                failed = True
            finally:
                ctypes.WinDLL("kernel32", use_last_error=True).CloseHandle(ctypes.c_void_p(handle))
        if failed:
            raise AssertionError("one or more owned fake-provider processes could not be stopped")


class Node:
    def __init__(self, package, workspace, name, provider):
        self.cli = package / "vibeface.exe"
        self.core = package / "vibeface-core.exe"
        self.directory = workspace / name
        self.provider = provider
        self.port = free_port()
        self.local_port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.process = None
        self.log = None
        self.http_device_id = None
        self.home = workspace / f"{name} home"
        self.home.mkdir(parents=True, exist_ok=True)
        run([
            self.cli, "init", "--data-dir", self.directory,
            "--listen", f"127.0.0.1:{self.port}",
            "--local-listen", f"127.0.0.1:{self.local_port}",
            "--root", str(workspace),
            "--bin", f"codex={provider.program}",
        ], timeout=120, env=self.environment())
        epoch = json.loads((self.directory / "data-epoch.json").read_text(encoding="utf-8"))
        require(epoch == {"data_epoch": DATA_EPOCH}, "new private node data uses the current data epoch")
        access = json.loads(run([
            self.cli, "access", "--json", "--data-dir", self.directory,
        ], env=self.environment()).stdout)
        require(access["device_id"], "local access pins the node identity")
        self.device_id = access["device_id"]

    def environment(self):
        base = {
            **os.environ,
            "USERPROFILE": str(self.home),
            "HOME": str(self.home),
            "CODEX_HOME": str(self.home / ".codex"),
        }
        return self.provider.environment(base)

    def configure(self, **values):
        path = self.directory / "config.json"
        configuration = json.loads(path.read_text(encoding="utf-8"))
        configuration.update(values)
        temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}")
        temporary.write_text(json.dumps(configuration, indent=2) + "\n", encoding="utf-8")
        os.replace(temporary, path)

    def use_fake_provider(self):
        configuration = json.loads((self.directory / "config.json").read_text(encoding="utf-8"))
        providers = configuration["providers"]
        providers["execution"] = ["codex"]
        providers["codex_app_server"] = {"mode": "spawn"}
        self.configure(providers=providers)

    def start(self):
        require(self.process is None, "the node is stopped before it is started")
        self.log = (self.directory / "process.log").open("ab")
        self.process = subprocess.Popen(
            [str(self.core), "--data-dir", str(self.directory), "--desktop-parent-stdin"],
            stdin=subprocess.PIPE, stdout=self.log, stderr=self.log,
            env=self.environment(),
        )

        def listener():
            if self.process.poll() is not None:
                raise AssertionError(
                    f"the node exited {self.process.returncode}; see {self.directory / 'process.log'}"
                )
            with urllib.request.urlopen(f"{self.url}/api/info", timeout=5) as response:
                return json.loads(response.read())

        info = wait_for("the node's local HTTP listener", listener)
        require(info["device_id"] == self.device_id, "the node serves its own device identity")
        self.http_device_id = info["device_id"]
        return info

    def read_status(self, timeout=60):
        return run(
            [self.cli, "status", "--json", "--data-dir", self.directory],
            check=False, timeout=timeout, env=self.environment(),
        )

    def followup_status_observation(self):
        try:
            result = self.read_status(timeout=15)
        except subprocess.TimeoutExpired:
            return {"command_state": "timeout"}
        except (OSError, ValueError):
            return {"command_state": "unavailable"}
        observation = status_observation(result, self)
        observation["returncode"] = result.returncode
        return observation

    def status(self, diagnose_runtime=False):
        result = self.read_status()
        try:
            status = json.loads(result.stdout)
        except ValueError:
            status = None
        state = status.get("status") if isinstance(status, dict) else None
        exit_codes = {"running": 0, "stopped": 1, "unavailable": 1}
        if (not isinstance(state, str) or state not in exit_codes or result.returncode != exit_codes[state]
                or diagnose_runtime and state != "running"):
            raise StatusCommandFailure(result, self, diagnose_runtime=diagnose_runtime)
        return status

    def ask_to_stop(self):
        require(self.process is not None, "the node is running before it is asked to stop")
        pid = self.process.pid
        require(self.process.stdin is not None, "the fixture owns the Core lifetime pipe")
        self.process.stdin.close()
        try:
            self.process.wait(timeout=45)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=15)
            raise AssertionError("the node did not stop after its owning desktop pipe closed")
        self.process = None
        if self.log:
            self.log.close()
            self.log = None
        wait_for("the node's own process to end", lambda: not process_is_running(pid), timeout=30)

    def kill(self):
        if self.process is not None and self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=15)
        self.process = None
        if self.log:
            self.log.close()
            self.log = None


def check_programs(package):
    cli = package / "vibeface.exe"
    core = package / "vibeface-core.exe"
    for program in (cli, core):
        require(program.is_file(), f"the package carries {program.name}")
    version = run([cli, "--version"]).stdout.decode().strip()
    core_version = run([core, "--version"]).stdout.decode().strip()
    require(version.startswith("vibeface "), f"the CLI reports its own name: {version}")
    require(core_version.startswith("vibeface-core "), f"Core reports its own name: {core_version}")
    return {"cli": version, "core": core_version}


def check_private_paths(node):
    require(owner_only(node.directory), "the data directory is private to this account")
    for name in ("config.json", "identity.json"):
        require(owner_only(node.directory / name), f"{name} is private to this account")


def check_private_endpoint(node, status):
    pipe = status["maintenance_socket"]
    require(
        pipe.startswith("\\\\.\\pipe\\") and "\\" not in pipe[len("\\\\.\\pipe\\"):],
        f"the maintenance endpoint is a named pipe on this machine: {pipe}",
    )
    probe = wait_for("the private maintenance endpoint", lambda: maintenance_probe(pipe), timeout=30)
    require(probe["status"] == "healthy", f"the maintenance endpoint answers: {probe}")
    require(
        probe.get("health", {}).get("device_id") == node.device_id,
        "the maintenance endpoint pins the node identity",
    )
    return pipe


def check_second_node_refused(node):
    result = run([node.cli, "serve", "--data-dir", node.directory], check=False, timeout=90,
                 env=node.environment())
    require(result.returncode != 0, "a second node on one data directory is refused")
    message = (result.stderr + result.stdout).decode(errors="replace").lower()
    require(
        "data directory" in message or "lock" in message or "already" in message,
        f"the refusal explains itself: {message[-300:]}",
    )


def check_provider_tree_is_cleared(node):
    pids = wait_for(
        "the fake provider and its own child to start",
        lambda: node.provider.pids() if len(node.provider.pids()) == 2 else None,
        timeout=60,
    )
    for name, (pid, created) in pids.items():
        handle, identity_error = open_fixture_process(pid, created, sys.executable)
        if handle is None:
            raise FixtureProcessCheckError(name.removesuffix(".pid"), identity_error)
        ctypes.WinDLL("kernel32", use_last_error=True).CloseHandle(ctypes.c_void_p(handle))
    node.ask_to_stop()
    for name, (pid, created) in pids.items():
        wait_for(
            f"the fake provider's {name} to be cleared with the node",
            lambda pid=pid, created=created:
                not fixture_process_is_running(pid, created, sys.executable),
            timeout=30,
        )
    return pids


def check_service_scripts(package, node, skip_service):
    shell = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    require(shell.is_file(), "Windows PowerShell is available")
    # The release package intentionally omits its bootstrap installer. Exercise the
    # repository source script directly so Windows acceptance still covers it.
    installer = Path(__file__).resolve().with_name("install.ps1")
    service = package / "scripts/install-node-service.ps1"
    checks = {}
    require(installer.is_file(), "the source Windows installer is available for its self-test")
    result = run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                  "-File", installer, "-SelfTest"], timeout=180)
    checks["installer_self_test"] = result.stdout.decode(errors="replace").strip()
    if not service.is_file():
        return checks
    usage = run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                 "-File", service, "--help"], timeout=120)
    checks["service_usage"] = "install-node-service.ps1" in usage.stdout.decode(errors="replace")
    if skip_service:
        return checks

    task_probe = r"""
$ErrorActionPreference = 'Stop'
try {
    $scheduler = New-Object -ComObject 'Schedule.Service'
    $scheduler.Connect()
    $task = $scheduler.GetFolder('\').GetTask('\Vibeface\Node')
    if (-not $task) { throw 'Task Scheduler returned no task object' }
    'present'
} catch {
    $failure = $_.Exception
    while ($failure.InnerException) { $failure = $failure.InnerException }
    if ($failure.HResult -eq -2147024894 -or $failure.HResult -eq -2147024893) {
        'absent'
        exit 0
    }
    [Console]::Error.WriteLine("Could not safely inspect the existing task (HRESULT $($failure.HResult)).")
    exit 1
}
"""
    task_probe_encoded = base64.b64encode(task_probe.encode("utf-16le")).decode("ascii")
    task_state = run([shell, "-NoProfile", "-NonInteractive", "-EncodedCommand", task_probe_encoded])
    task_state_text = task_state.stdout.decode("utf-8-sig").strip().lower()
    if task_state_text == "present":
        raise IncompleteAcceptance("service task coverage incomplete; existing task preserved")
    require(task_state_text == "absent", "existing service task state is unambiguous")

    def service_call(*arguments, check=True):
        return run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                    "-File", service, *arguments], timeout=180, check=check)
    try:
        require(node.status()["status"] == "stopped",
                "service installation starts from a stopped fixture node")
        service_call("install", "--binary", package / "vibeface.exe",
                     "--data-dir", node.directory)
        query = run([Path(os.environ["SystemRoot"]) / "System32/schtasks.exe",
                     "/Query", "/TN", "\\Vibeface\\Node", "/XML", "ONE"], timeout=60)
        definition = query.stdout.decode("utf-16", errors="replace")
        if "<Task" not in definition:
            definition = query.stdout.decode("utf-8", errors="replace")
        require("serve" in definition and str(node.directory) in definition,
                "the registered task starts this node on this data directory")
        other = node.directory.parent / "another node"
        other.mkdir(parents=True, exist_ok=True)
        for name in ("config.json", "identity.json"):
            (other / name).write_text("{}", encoding="utf-8")
        refused = service_call("stop", "--data-dir", other, check=False)
        require(refused.returncode != 0,
                "a service command naming another data directory is refused")
        wait_for("the scheduled task to start this node",
                 lambda: node.status() if node.status().get("status") == "running" else None)
        service_call("stop", "--data-dir", node.directory)
        wait_for("the scheduled task to stop this node",
                 lambda: node.status() if node.status().get("status") == "stopped" else None)
        service_call("start", "--data-dir", node.directory)
        wait_for("the scheduled task to restart this node",
                 lambda: node.status() if node.status().get("status") == "running" else None)
        checks["service_task"] = True
    finally:
        service_call("uninstall", "--data-dir", node.directory, check=False)
    wait_for("the scheduled task to leave the node stopped",
             lambda: node.status() if node.status().get("status") == "stopped" else None)
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", required=True, type=Path,
                        help="directory holding vibeface.exe and vibeface-core.exe")
    parser.add_argument("--artifacts", type=Path, default=None,
                        help="where to write the result summary")
    parser.add_argument("--keep", action="store_true", help="keep the scratch directory")
    parser.add_argument("--skip-service", action="store_true",
                        help="check the service script without registering a task")
    options = parser.parse_args()
    require(os.name == "nt", "this check runs on Windows")
    package = options.package.resolve()
    checks = {"programs": check_programs(package)}

    scratch = Path(tempfile.mkdtemp(prefix="vibeface-win-"))
    workspace = scratch / WORKSPACE_NAME
    workspace.mkdir(parents=True)
    node = None
    failure = None
    try:
        provider = FakeProvider(workspace / "fake provider")
        node = Node(package, workspace, NODE_NAME, provider)
        check_private_paths(node)
        node.start()
        status = node.status(diagnose_runtime=True)
        require(status["status"] == "running", f"status reports a running node: {status}")
        check_private_endpoint(node, status)
        checks["pipe"] = "owner-only named pipe probe passed"
        check_second_node_refused(node)
        node.ask_to_stop()
        checks["restart"] = node.start()["device_id"] == node.device_id
        node.ask_to_stop()

        node.use_fake_provider()
        node.start()
        check_provider_tree_is_cleared(node)
        checks["provider_tree"] = "child process tree cleared"
        checks["provider_arguments"] = json.loads((provider.marks / "argv.json").read_text())
        checks["service"] = check_service_scripts(package, node, options.skip_service)
    except BaseException as error:
        failure = error
        if options.artifacts:
            options.artifacts.mkdir(parents=True, exist_ok=True)
            # Assertions can include structured runtime values. Keep those out of artifacts;
            # the isolated fixture process log below carries the actionable local context.
            diagnostic = {
                "status": "incomplete" if isinstance(error, IncompleteAcceptance) else "failed",
                "error_type": type(error).__name__,
            }
            if isinstance(error, FixtureProcessCheckError):
                diagnostic["fixture_process_check"] = {
                    "role": error.role, "result": error.reason,
                }
            if isinstance(error, StatusCommandFailure):
                diagnostic["status_command"] = {
                    "returncode": error.returncode,
                    "category": error.category,
                    "stderr_excerpt": error.stderr,
                    "node_status": error.node_status,
                    "probe_error": error.probe_error,
                    "runtime_diagnostics": error.runtime_diagnostics,
                }
            (options.artifacts / "windows-node-failure.json").write_text(
                json.dumps(diagnostic, indent=2) + "\n", encoding="utf-8"
            )
            # This log belongs to the isolated fake-provider fixture. Do not archive
            # config, identity, status, or command output that may contain identifiers.
            if node is not None and node.log:
                node.log.flush()
            process_log = node.directory / "process.log" if node is not None else None
            if process_log and process_log.is_file():
                shutil.copyfile(process_log, options.artifacts / "windows-node-process.log")
    finally:
        cleanup_errors = []
        if node is not None:
            try:
                node.kill()
            except Exception as error:
                cleanup_errors.append(type(error).__name__)
            if hasattr(node, "provider"):
                try:
                    node.provider.stop_children()
                except Exception as error:
                    cleanup_errors.append(type(error).__name__)
        if cleanup_errors:
            if failure is None:
                failure = AssertionError("fixture process cleanup did not complete")
            if options.artifacts:
                options.artifacts.mkdir(parents=True, exist_ok=True)
                diagnostic_path = options.artifacts / "windows-node-failure.json"
                diagnostic = {"status": "failed", "error_type": type(failure).__name__}
                if diagnostic_path.is_file():
                    try:
                        diagnostic.update(json.loads(diagnostic_path.read_text(encoding="utf-8")))
                    except (OSError, ValueError):
                        pass
                diagnostic["cleanup_error_types"] = cleanup_errors
                diagnostic_path.write_text(json.dumps(diagnostic, indent=2) + "\n", encoding="utf-8")
        if not options.keep:
            shutil.rmtree(scratch, ignore_errors=True)

    if failure is not None:
        raise failure

    summary = {"status": "passed", "checks": checks, "real_provider_calls": 0}
    if options.artifacts:
        options.artifacts.mkdir(parents=True, exist_ok=True)
        (options.artifacts / "windows-node.json").write_text(
            json.dumps(summary, indent=2, default=str) + "\n", encoding="utf-8"
        )
    print(json.dumps(summary, indent=2, default=str), flush=True)


if __name__ == "__main__":
    try:
        main()
    except IncompleteAcceptance as error:
        print(json.dumps({"status": "incomplete", "error": str(error)}), file=sys.stderr, flush=True)
        sys.exit(2)
    except AssertionError as error:
        print(json.dumps({"status": "failed", "error": str(error)}), file=sys.stderr, flush=True)
        sys.exit(1)
