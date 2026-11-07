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
import signal
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


def open_fixture_process(pid, created):
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
        return None
    try:
        code = ctypes.c_ulong()
        times = [FILETIME() for _ in range(4)]
        if (not kernel.GetExitCodeProcess(handle, ctypes.byref(code)) or
                code.value != STILL_RUNNING or
                not kernel.GetProcessTimes(handle, *(ctypes.byref(value) for value in times))):
            kernel.CloseHandle(handle)
            return None
        actual_created = (times[0].high << 32) | times[0].low
        image = ctypes.create_unicode_buffer(32768)
        length = ctypes.c_ulong(len(image))
        if (actual_created != created or
                not kernel.QueryFullProcessImageNameW(handle, 0, image, ctypes.byref(length)) or
                os.path.normcase(os.path.abspath(image.value)) !=
                os.path.normcase(os.path.abspath(sys.executable))):
            kernel.CloseHandle(handle)
            return None
        return handle
    except BaseException:
        kernel.CloseHandle(handle)
        raise


def fixture_process_is_running(pid, created):
    handle = open_fixture_process(pid, created)
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
            handle = open_fixture_process(pid, created)
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
        self.directory = workspace / name
        self.provider = provider
        self.port = free_port()
        self.local_port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.process = None
        self.log = None
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
            [str(self.cli), "serve", "--data-dir", str(self.directory)],
            stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
            env=self.environment(),
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
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
        return info

    def status(self):
        result = run([self.cli, "status", "--json", "--data-dir", self.directory], check=False,
                     env=self.environment())
        require(result.returncode == 0, "status inspects the initialized node")
        return json.loads(result.stdout)

    def ask_to_stop(self):
        require(self.process is not None, "the node is running before it is asked to stop")
        pid = self.process.pid
        os.kill(pid, signal.CTRL_BREAK_EVENT)
        try:
            self.process.wait(timeout=45)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=15)
            raise AssertionError("the node did not stop when the console asked it to")
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
        require(fixture_process_is_running(pid, created),
                f"the fake provider's {name} is running before the stop")
    node.ask_to_stop()
    for name, (pid, created) in pids.items():
        wait_for(
            f"the fake provider's {name} to be cleared with the node",
            lambda pid=pid, created=created: not fixture_process_is_running(pid, created),
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
Import-Module ScheduledTasks
try {
    $task = Get-ScheduledTask -TaskName 'Node' -TaskPath '\Vibeface\' -ErrorAction Stop
    if (-not $task) { [Console]::Error.WriteLine('Could not confirm task state.'); exit 1 }
    'present'
} catch {
    if ($_.FullyQualifiedErrorId -like 'NoMSFT_ScheduledTaskObjectsFound,*') {
        'absent'
        exit 0
    }
    [Console]::Error.WriteLine('Could not safely inspect the existing task.')
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
        status = node.status()
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
