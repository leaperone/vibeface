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
import ntpath
import os
import re
import shlex
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
import xml.etree.ElementTree as ET
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


def config_acl_comparison(value):
    """Keep only bounded ACL facts, never paths, file contents or PowerShell error text."""
    require(isinstance(value, dict), "fixture ACL diagnostic must be an object")
    result = {}
    for label in ("original", "verified"):
        source = value.get(label)
        require(isinstance(source, dict), "fixture ACL diagnostic is missing a descriptor")
        owner = source.get("owner")
        sddl = source.get("sddl")
        require(isinstance(owner, str) and re.fullmatch(r"S-\d+(?:-\d+)+", owner)
                and len(owner) <= 256, "fixture ACL diagnostic owner is not a SID")
        require(isinstance(sddl, str) and re.fullmatch(r"[A-Za-z0-9_:;(){}.,\-]*", sddl)
                and len(sddl) <= 1536, "fixture ACL diagnostic is not a bounded owner/access SDDL")
        fields = {"owner": owner, "sddl": sddl}
        for name in ("control_flags", "rule_count", "inherited_count"):
            number = source.get(name)
            require(type(number) is int and 0 <= number <= 65535,
                    "fixture ACL diagnostic count or flags are invalid")
            fields[name] = number
        for name in ("protected", "canonical", "dacl_present", "dacl_auto_inherited",
                     "dacl_auto_inherit_required", "dacl_defaulted", "owner_defaulted"):
            require(type(source.get(name)) is bool, "fixture ACL diagnostic flag is invalid")
            fields[name] = source[name]
        rules = source.get("rules")
        require(isinstance(rules, list) and len(rules) <= 8,
                "fixture ACL diagnostic rule sample is invalid")
        fields["rules"] = []
        for rule in rules:
            require(isinstance(rule, dict) and isinstance(rule.get("sid"), str)
                    and re.fullmatch(r"S-\d+(?:-\d+)+", rule["sid"])
                    and len(rule["sid"]) <= 256, "fixture ACL diagnostic trustee is not a SID")
            normalized = {"sid": rule["sid"]}
            for name in ("access_type", "rights", "inheritance", "propagation"):
                number = rule.get(name)
                require(type(number) is int and -(2 ** 31) <= number < 2 ** 32,
                        "fixture ACL diagnostic rule mask is invalid")
                normalized[name] = number
            require(type(rule.get("inherited")) is bool,
                    "fixture ACL diagnostic inherited flag is invalid")
            normalized["inherited"] = rule["inherited"]
            fields["rules"].append(normalized)
        result[label] = fields
    return result


def copy_private_config_security(source, temporary):
    """Copy validated owner/DACL onto this fixture's empty replacement before writing data."""
    require(source.name == "config.json", "only the fixture config may be replaced")
    require(temporary.parent == source.parent
            and re.fullmatch(r"\.config\.json\.[0-9a-f]{32}", temporary.name),
            "the replacement is this fixture's fresh same-directory config")
    script = r"""
$ErrorActionPreference = 'Stop'
$sidType = [System.Security.Principal.SecurityIdentifier]
$current = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$sections = [System.Security.AccessControl.AccessControlSections]::Owner -bor [System.Security.AccessControl.AccessControlSections]::Access
function Read-PrivateAcl([string] $path, [bool] $directory) {
    $item = Get-Item -LiteralPath $path -Force
    if ($item.PSIsContainer -ne $directory -or
        ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw 'fixture config paths must be ordinary files and directories'
    }
    $acl = Get-Acl -LiteralPath $path
    $rules = @($acl.GetAccessRules($true, $true, $sidType))
    $allow = @($rules | Where-Object {
        $_.AccessControlType -eq [System.Security.AccessControl.AccessControlType]::Allow
    })
    if ($acl.GetOwner($sidType).Value -ne $current -or
        $allow.Count -eq 0 -or
        @($allow | Where-Object { $_.IdentityReference.Value -ne $current }).Count -ne 0 -or
        -not $acl.AreAccessRulesProtected -or
        @($rules | Where-Object { $_.IsInherited }).Count -ne 0) {
        throw 'fixture config must already be protected for the current Windows SID'
    }
    return $acl
}
$source = [System.IO.Path]::GetFullPath($env:VIBEFACE_ACL_FIXTURE)
$temporary = [System.IO.Path]::GetFullPath($env:VIBEFACE_ACL_TEMPORARY)
$parent = [System.IO.Path]::GetDirectoryName($source)
if (-not [StringComparer]::OrdinalIgnoreCase.Equals($parent, [System.IO.Path]::GetDirectoryName($temporary)) -or
    [System.IO.Path]::GetFileName($source) -cne 'config.json' -or
    [System.IO.Path]::GetFileName($temporary) -cnotmatch '^\.config\.json\.[0-9a-f]{32}$') {
    throw 'fixture config security may only be copied to its same-directory replacement'
}
$null = Read-PrivateAcl $parent $true
$original = Read-PrivateAcl $source $false
$item = Get-Item -LiteralPath $temporary -Force
if ($item.PSIsContainer -or $item.Length -ne 0 -or
    ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
    throw 'fixture replacement must be a fresh empty ordinary file'
}
$replacement = Get-Acl -LiteralPath $temporary
if ($replacement.GetOwner($sidType).Value -ne $current) {
    throw 'fixture replacement belongs to another Windows SID'
}
$descriptor = $original.GetSecurityDescriptorSddlForm($sections)
$replacement.SetSecurityDescriptorSddlForm($descriptor, $sections)
Set-Acl -LiteralPath $temporary -AclObject $replacement
$verified = Read-PrivateAcl $temporary $false
if ($verified.GetSecurityDescriptorSddlForm($sections) -cne $descriptor) {
    function Read-AclFacts($acl) {
        $raw = [System.Security.AccessControl.RawSecurityDescriptor]::new($acl.GetSecurityDescriptorBinaryForm(), 0)
        $flags = [int] $raw.ControlFlags
        $rules = @($acl.GetAccessRules($true, $true, $sidType))
        $normalized = @($rules | Select-Object -First 8 | ForEach-Object {
            @{sid=$_.IdentityReference.Value; access_type=[int]$_.AccessControlType;
              rights=[long]$_.FileSystemRights; inheritance=[int]$_.InheritanceFlags;
              propagation=[int]$_.PropagationFlags; inherited=[bool]$_.IsInherited}
        })
        return @{owner=$acl.GetOwner($sidType).Value;
            sddl=$acl.GetSecurityDescriptorSddlForm($sections);
            control_flags=$flags; protected=[bool]$acl.AreAccessRulesProtected;
            canonical=[bool]$acl.AreAccessRulesCanonical; rule_count=$rules.Count;
            inherited_count=@($rules | Where-Object {$_.IsInherited}).Count;
            dacl_present=(($flags -band 4) -ne 0);
            dacl_defaulted=(($flags -band 8) -ne 0);
            dacl_auto_inherit_required=(($flags -band 256) -ne 0);
            dacl_auto_inherited=(($flags -band 1024) -ne 0);
            owner_defaulted=(($flags -band 1) -ne 0); rules=$normalized}
    }
    $facts = @{original=(Read-AclFacts $original); verified=(Read-AclFacts $verified)}
    [Console]::Out.WriteLine('VIBEFACE_CONFIG_ACL_COMPARE ' + ($facts | ConvertTo-Json -Depth 6 -Compress))
    throw 'fixture replacement did not preserve the original owner and protected DACL'
}
"""
    encoded = base64.b64encode(script.encode('utf-16le')).decode('ascii')
    result = run(['powershell.exe', '-NoProfile', '-NonInteractive', '-EncodedCommand', encoded],
                 check=False,
                 env={**os.environ, 'VIBEFACE_ACL_FIXTURE': str(source),
                      'VIBEFACE_ACL_TEMPORARY': str(temporary)})
    if result.returncode != 0:
        # PowerShell's CLIXML stderr is path-bearing and run() truncates it. Preserve only
        # the safe structured comparison emitted above, without changing any ACL assertion.
        marker = 'VIBEFACE_CONFIG_ACL_COMPARE '
        output = result.stdout[:16384].decode('utf-8-sig', errors='replace')
        for line in output.splitlines():
            if line.startswith(marker):
                facts = config_acl_comparison(json.loads(line[len(marker):]))
                raise AssertionError('fixture replacement did not preserve the original owner '
                                     'and protected DACL; ACL comparison: '
                                     + json.dumps(facts, separators=(',', ':'), ensure_ascii=True))
        raise AssertionError('fixture config security validation or copy failed')


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
            "--no-mesh",
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
        configuration = json.loads((self.directory / "config.json").read_text(encoding="utf-8"))
        network = configuration["network"]
        # A free TCP port may be excluded for UDP on Windows. Keep real QUIC on
        # loopback and let its UDP bind allocate a port, independently of HTTP.
        network["bind"] = "127.0.0.1:0"
        self.configure(network=network)

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
        for candidate, expected in ((self.directory, stat.S_ISDIR), (path, stat.S_ISREG)):
            metadata = candidate.lstat()
            require(expected(metadata.st_mode)
                    and not getattr(metadata, "st_file_attributes", 0) & 0x400,
                    "fixture config and its directory are ordinary paths, never reparse points")
        temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}")
        claimed = False
        try:
            # Claim an empty file exclusively; it contains no config before its ACL is private.
            with temporary.open("x", encoding="utf-8"):
                claimed = True
            copy_private_config_security(path, temporary)
            configuration = json.loads(path.read_text(encoding="utf-8"))
            configuration.update(values)
            with temporary.open("w", encoding="utf-8", newline="\n") as output:
                output.write(json.dumps(configuration, indent=2) + "\n")
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, path)
            require(owner_only(path), "the updated fixture config preserves its private owner and DACL")
        finally:
            if claimed:
                temporary.unlink(missing_ok=True)

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

    def core_process_identities(self):
        """Read identities for this fixture's Core processes without logging raw process data."""
        script = r"""
$ErrorActionPreference = 'Stop'
$image = [System.IO.Path]::GetFullPath($env:VIBEFACE_EXPECTED_CORE_IMAGE)
$data = $env:VIBEFACE_EXPECTED_NODE_DATA
$coreProcesses = @(Get-CimInstance -ClassName Win32_Process -Filter "Name='vibeface-core.exe'" |
    Where-Object {
        $_.ExecutablePath -and
        [String]::Equals([System.IO.Path]::GetFullPath($_.ExecutablePath), $image,
            [StringComparison]::OrdinalIgnoreCase) -and
        $_.CommandLine -and
        $_.CommandLine.IndexOf($data, [StringComparison]::OrdinalIgnoreCase) -ge 0
    })
Write-Output "count=$($coreProcesses.Count)"
foreach ($process in $coreProcesses) {
    Write-Output "identity=$([string]$process.ProcessId)|$([string]$process.CreationDate)"
}
"""
        encoded = base64.b64encode(script.encode("utf-16le")).decode("ascii")
        environment = {
            **self.environment(),
            "VIBEFACE_EXPECTED_CORE_IMAGE": str(self.core.resolve(strict=True)),
            "VIBEFACE_EXPECTED_NODE_DATA": str(self.directory.resolve(strict=True)),
        }
        try:
            result = subprocess.run(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-EncodedCommand", encoded],
                capture_output=True, timeout=15, env=environment,
            )
        except (OSError, subprocess.TimeoutExpired):
            return None
        if result.returncode != 0:
            return None
        lines = result.stdout.decode("utf-8-sig", errors="replace").splitlines()
        count_line = next((line for line in lines if line.startswith("count=")), None)
        identities = tuple(line.removeprefix("identity=") for line in lines
                           if line.startswith("identity="))
        try:
            count = int(count_line.split("=", 1)[1]) if count_line else -1
        except ValueError:
            return None
        return identities if count == len(identities) else None

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


def service_task_xml_encoding(data):
    if data.startswith(b"\xff\xfe"):
        return "utf-16-le-bom"
    if data.startswith(b"\xfe\xff"):
        return "utf-16-be-bom"
    if data.startswith(b"\xef\xbb\xbf"):
        return "utf-8-bom"
    if data.startswith(b"<\x00?\x00"):
        return "utf-16-le"
    if data.startswith(b"\x00<\x00?"):
        return "utf-16-be"
    declaration = data[:160].decode("ascii", errors="ignore").lower()
    match = re.search(r"encoding=['\"]([^'\"]+)", declaration)
    return match.group(1).lower() if match else "no-bom"


def service_task_action_matches(data, binary, data_dir):
    """Check the generated Exec action while returning only sanitized match facts."""
    summary = {
        "encoding": service_task_xml_encoding(data),
        "xml_parse": False,
        "action_count": 0,
        "exec_action_count": 0,
        "command_matches": False,
        "arguments_match": False,
        "working_directory_matches": False,
    }
    try:
        document = ET.fromstring(data)
    except (ET.ParseError, ValueError):
        return summary
    summary["xml_parse"] = True
    namespace = "{http://schemas.microsoft.com/windows/2004/02/mit/task}"
    action_container = document.find(f".//{namespace}Actions")
    summary["action_count"] = len(action_container) if action_container is not None else 0
    actions = document.findall(f".//{namespace}Actions/{namespace}Exec")
    summary["exec_action_count"] = len(actions)
    if summary["action_count"] != 1 or len(actions) != 1:
        return summary
    action = actions[0]
    command = action.findtext(f"{namespace}Command") or ""
    arguments = action.findtext(f"{namespace}Arguments") or ""
    working_directory = action.findtext(f"{namespace}WorkingDirectory") or ""

    def canonical(path):
        if not path:
            return None
        try:
            if os.name == "nt":
                return ntpath.normcase(str(Path(path).resolve(strict=True)))
            return ntpath.normcase(ntpath.normpath(path))
        except (OSError, RuntimeError):
            return None

    def same_existing_path(actual, expected):
        actual_path = canonical(actual)
        expected_path = canonical(expected)
        return actual_path is not None and actual_path == expected_path

    try:
        tokens = shlex.split(arguments, posix=False)
    except ValueError:
        tokens = []
    data_argument = tokens[2].strip('"') if len(tokens) == 3 else ""
    summary["command_matches"] = same_existing_path(command, str(binary))
    summary["arguments_match"] = (
        len(tokens) == 3
        and tokens[0] == "serve"
        and tokens[1] == "--data-dir"
        and same_existing_path(data_argument, str(data_dir))
    )
    summary["working_directory_matches"] = (
        same_existing_path(working_directory, ntpath.dirname(str(binary)))
    )
    return summary


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
function Read-TaskState($scheduler, [string] $name) {
    try {
        $null = $scheduler.GetFolder('\').GetTask($name)
        return 'present'
    } catch {
        $failure = $_.Exception
        while ($failure.InnerException) { $failure = $failure.InnerException }
        if ($failure.HResult -eq -2147024894 -or $failure.HResult -eq -2147024893) {
            return 'absent'
        }
        throw
    }
}
try {
    $scheduler = New-Object -ComObject 'Schedule.Service'
    $scheduler.Connect()
    $node = Read-TaskState $scheduler '\Vibeface\Node'
    $updater = Read-TaskState $scheduler '\Vibeface\Update'
    @{node=$node;updater=$updater} | ConvertTo-Json -Compress
} catch {
    $failure = $_.Exception
    while ($failure.InnerException) { $failure = $failure.InnerException }
    [Console]::Error.WriteLine("Could not safely inspect service task state (HRESULT $($failure.HResult)).")
    exit 1
}
"""
    task_probe_encoded = base64.b64encode(task_probe.encode("utf-16le")).decode("ascii")

    def service_task_states():
        result = run([shell, "-NoProfile", "-NonInteractive", "-EncodedCommand", task_probe_encoded],
                     check=False, timeout=60)
        if result.returncode != 0:
            return None
        try:
            states = json.loads(result.stdout.decode("utf-8-sig"))
        except ValueError:
            return None
        if (not isinstance(states, dict)
                or set(states) != {"node", "updater"}
                or any(states[name] not in ("absent", "present") for name in states)):
            return None
        return states

    absent_tasks = {"node": "absent", "updater": "absent"}
    if service_task_states() != absent_tasks:
        raise IncompleteAcceptance("service task coverage incomplete; existing task state preserved")

    def tasks_are_absent():
        states = service_task_states()
        return states if states == absent_tasks else None

    def registered_node_action():
        # schtasks can emit console-encoded bytes with an unchanged UTF-16 XML declaration.
        # Read the scheduler's Unicode definition and transport it as ASCII base64 instead.
        script = r"""
$ErrorActionPreference = 'Stop'
try {
    $scheduler = New-Object -ComObject 'Schedule.Service'
    $scheduler.Connect()
    $definition = $scheduler.GetFolder('\').GetTask('\Vibeface\Node').Xml
    [Console]::Out.WriteLine([Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($definition)))
} catch {
    $failure = $_.Exception
    while ($failure.InnerException) { $failure = $failure.InnerException }
    [Console]::Error.WriteLine("Could not read task definition (HRESULT $($failure.HResult)).")
    exit 1
}
"""
        encoded = base64.b64encode(script.encode("utf-16le")).decode("ascii")
        result = run([
            shell, "-NoProfile", "-NonInteractive", "-EncodedCommand", encoded,
        ], check=False, timeout=60)
        if result.returncode != 0:
            summary = service_task_action_matches(b"", node.cli, node.directory)
            summary["query_succeeded"] = False
            summary["encoding"] = "unknown"
            return summary
        try:
            definition = base64.b64decode(result.stdout.strip(), validate=True)
        except ValueError:
            definition = b""
        summary = service_task_action_matches(definition, node.cli, node.directory)
        summary["query_succeeded"] = True
        return summary

    def service_call(*arguments, check=True):
        return run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                    "-File", service, *arguments], timeout=180, check=check)

    def running_service_instance():
        status = node.status()
        if status.get("status") != "running" or status.get("device_id") != node.device_id:
            return None
        identities = node.core_process_identities()
        if identities is None or len(identities) != 1:
            return None
        return identities[0], status["device_id"]

    try:
        require(node.status()["status"] == "stopped",
                "service installation starts from a stopped fixture node")
        service_call("install", "--binary", package / "vibeface.exe",
                     "--data-dir", node.directory)
        action = registered_node_action()
        require(
            action["query_succeeded"] and action["xml_parse"]
            and action["action_count"] == 1 and action["exec_action_count"] == 1
            and action["command_matches"]
            and action["arguments_match"] and action["working_directory_matches"],
            f"registered task action matches expected verb/path (encoding={action['encoding']}, "
            f"query={action['query_succeeded']}, parse={action['xml_parse']}, "
            f"actions={action['action_count']}, "
            f"exec_actions={action['exec_action_count']}, command={action['command_matches']}, "
            f"arguments={action['arguments_match']}, "
            f"working_directory={action['working_directory_matches']})",
        )
        other = node.directory.parent / "another node"
        other.mkdir(parents=True, exist_ok=True)
        for name in ("config.json", "identity.json"):
            (other / name).write_text("{}", encoding="utf-8")
        refused = service_call("stop", "--data-dir", other, check=False)
        require(refused.returncode != 0,
                "a service command naming another data directory is refused")
        wait_for("the scheduled task to start this node",
                 lambda: node.status() if node.status().get("status") == "running" else None)
        before_restart = wait_for(
            "the service Core process identity",
            running_service_instance,
        )
        service_call("restart", "--data-dir", node.directory)

        def restarted_service_instance():
            current = running_service_instance()
            return current if current and current[0] != before_restart[0] else None

        after_restart = wait_for(
            "the service restart to create a new Core process with the same device identity",
            restarted_service_instance,
        )
        require(
            after_restart[0] != before_restart[0]
            and after_restart[1] == before_restart[1] == node.device_id,
            "service restart changes process identity but preserves device identity",
        )
        service_call("stop", "--data-dir", node.directory)
        wait_for("the scheduled task to stop this node",
                 lambda: node.status() if node.status().get("status") == "stopped" else None)
        service_call("start", "--data-dir", node.directory)
        wait_for("the scheduled task to restart this node",
                 lambda: node.status() if node.status().get("status") == "running" else None)
        checks["service_task"] = True
    finally:
        cleanup = service_call("uninstall", "--data-dir", node.directory, check=False)
        if cleanup.returncode != 0 and sys.exc_info()[0] is None:
            raise AssertionError("service uninstall failed after lifecycle checks")
    require(cleanup.returncode == 0, "service uninstall returned success")
    wait_for("the node and updater tasks to be absent",
             tasks_are_absent)
    service_call("uninstall", "--data-dir", node.directory)
    wait_for("repeated uninstall leaves both tasks absent", tasks_are_absent)
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
