#!/usr/bin/env python3
"""Join genuine paid proofs with real Web and Android delivery in one bounded run."""

import argparse
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import re
import runpy
import signal
import shutil
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.parse
import uuid
import zipfile


ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / ".artifacts/vdp"
PUBLIC = ARTIFACTS / "live-public"
UI = ARTIFACTS / "live-ui"
UI_TIMEOUT_MS = 3_600_000
LIVE_TIMEOUT_SECONDS = 20_700
PROOFS = ["key-binding", *[f"relation-{relation}-window-{index}" for index in range(2) for relation in (2, 3, 4)]]
ANDROID_ABIS = {"arm64-v8a": 183, "x86_64": 62}
UI_EVENTS = ("prepared", "confirmed", "accepted", "offline-visible", "settled-offline", "saved")
UI_FILES = ("web.json", "android.json", "web.png", "android-offline.png", "android.png")
INSPECTIONS = (("receiver-key-before", "android", "android-key-before.png"),
               ("source-partial", "web", "web-partial-refund.png"),
               ("source-unknown", "web", "web-unknown-transaction.png"),
               ("receiver-key-after", "android", "android-key-rotated.png"))
VIDEO_SPECS = {"web": (("web-confirm.mp4", "web-confirm", "ffmpeg-x11grab"),),
               "android": (("android-accept-offline.mp4", "android-accept-offline", "adb-screenrecord"),
                           ("android-resume-save.mp4", "android-resume-save", "adb-screenrecord"))}
BRANCH_CASES = ("offline-refund", "partial-refund", "held-timeout", "strict-late-open", "late-priority", "hold-reorg", "capsule-reorg", "keeper",
                "token-false", "token-frozen", "token-fee", "token-sender-tax", "token-precision", "token-nonreturn", "token-reentrancy", "node-refund-hook",
                "incoming-false", "incoming-frozen", "incoming-fee", "incoming-sender-tax", "incoming-precision", "incoming-reentrancy", "incoming-nonreturn", "mixed-held-rotate")


def digest(file):
    value = hashlib.sha256()
    with file.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def regular(file, limit=1024 * 1024):
    if file.is_symlink() or not file.is_file() or not 0 < file.stat().st_size <= limit:
        raise ValueError("Expected a bounded regular input file")
    return file.read_bytes()


def write_private(file, value):
    descriptor = os.open(file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as output:
        json.dump(value, output, indent=2)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())


def source_commit(expected):
    if not re.fullmatch(r"[0-9a-f]{40}", expected):
        raise ValueError("Expected an explicit full source commit")
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    dirty = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=ROOT, text=True)
    if actual != expected or dirty:
        details = {"expected": expected, "actual": actual, "changes": dirty.splitlines()[:20]}
        raise ValueError("Live delivery requires the clean exact checkout used by the artifact job: " + json.dumps(details))


def checked_package(package, expected):
    info = json.loads(regular(package / "BUILD-INFO.json"))
    if set(info) != {"version", "target", "source_commit", "update_protocol", "data_epoch"} or info["source_commit"] != expected or info["target"] != "x86_64-unknown-linux-gnu":
        raise ValueError("Package provenance differs from this Linux source commit")
    if not (package / "vibeface").is_file() or (package / "vibeface").is_symlink():
        raise ValueError("Package Node binary is missing")
    for name in ("vibeface_secure.js", "vibeface_secure_bg.wasm"):
        generated = ROOT / "crates/node/web/generated" / name
        if digest(package / "web" / name) != digest(generated):
            raise ValueError("Packaged cryptography differs from the same-run verified Web/WASM artifact")
    return info


def unpack_package(directory):
    archives = list(directory.glob("*.tar.gz"))
    if len(archives) != 1:
        raise ValueError("Exactly one same-run Linux package archive is required")
    archive = archives[0]
    checksum = regular(archive.with_name(archive.name + ".sha256")).decode().strip().split()
    if len(checksum) != 2 or checksum[0] != digest(archive) or checksum[1].lstrip("*") != archive.name:
        raise ValueError("Linux package archive checksum mismatch")
    destination = ARTIFACTS / "live-input/unpacked"
    destination.mkdir(parents=True, exist_ok=False)
    with tarfile.open(archive) as contents:
        members = contents.getmembers()
        roots = set()
        for member in members:
            name = Path(member.name)
            if name.is_absolute() or ".." in name.parts or not (member.isfile() or member.isdir()):
                raise ValueError("Package contains an unexpected archive entry")
            roots.add(name.parts[0])
        if len(roots) != 1:
            raise ValueError("Package must have a single root")
        contents.extractall(destination)
    return destination / roots.pop()


def android_tool(name):
    candidates = [shutil.which(name)]
    for variable in ("ANDROID_SDK_ROOT", "ANDROID_HOME"):
        if os.environ.get(variable):
            candidates.append(str(Path(os.environ[variable]) / "cmdline-tools/latest/bin" / name))
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return str(Path(candidate).resolve())
    raise ValueError("The Android command-line tools are required")


def apk_native_libraries(apk):
    if apk.is_symlink() or not apk.is_file() or not 0 < apk.stat().st_size <= 1024 ** 3:
        raise ValueError("Expected one bounded regular Android release APK")
    libraries = []
    with zipfile.ZipFile(apk) as archive:
        for abi, machine in ANDROID_ABIS.items():
            name = f"lib/{abi}/libvibeface-secure-native.so"
            if archive.namelist().count(name) != 1 or not 20 <= archive.getinfo(name).file_size <= 64 * 1024 ** 2:
                raise ValueError("Release APK requires exactly one bounded native security library per ABI")
            with archive.open(name) as source:
                header = source.read(20)
                if header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[18:20], "little") != machine:
                    raise ValueError("Release APK contains a foreign or invalid native security ABI")
                value = hashlib.sha256(header)
                for block in iter(lambda: source.read(65536), b""):
                    value.update(block)
            libraries.append({"file": name, "sha256": value.hexdigest()})
    return libraries


def record_android(directory, expected):
    source_commit(expected)
    metadata = json.loads(regular(directory / "output-metadata.json"))
    if metadata.get("variantName") != "release" or metadata.get("applicationId") != "one.leaper.vibeface" or len(metadata.get("elements", [])) != 1:
        raise ValueError("Expected this source build's single Android release variant")
    apk = directory / metadata["elements"][0]["outputFile"]
    if apk.parent != directory or not re.fullmatch(r"[A-Za-z0-9._-]+\.apk", apk.name):
        raise ValueError("Expected a regular release APK basename")
    analyzer = android_tool("apkanalyzer")
    if subprocess.check_output([analyzer, "manifest", "application-id", str(apk)], text=True).strip() != "one.leaper.vibeface" or subprocess.check_output([analyzer, "manifest", "debuggable", str(apk)], text=True).strip() != "false":
        raise ValueError("Actual APK manifest is not the expected nondebuggable application")
    output = ROOT / ".artifacts/mobile-android"
    output.mkdir(parents=True, exist_ok=False)
    entry = {"file": apk.name, "sha256": digest(apk), "abis": list(ANDROID_ABIS), "nativeLibraries": apk_native_libraries(apk)}
    shutil.copyfile(apk, output / apk.name)
    (output / (apk.name + ".sha256")).write_text(entry["sha256"] + "  " + apk.name + "\n")
    write_private(output / "provenance.json", {"schemaVersion": 1, "sourceCommit": expected, "variant": "release", "applicationId": "one.leaper.vibeface", "apks": [entry]})


def checked_android(directory, expected):
    value = json.loads(regular(directory / "provenance.json"))
    if set(value) != {"schemaVersion", "sourceCommit", "variant", "applicationId", "apks"} or value["schemaVersion"] != 1 or value["sourceCommit"] != expected or value["variant"] != "release" or value["applicationId"] != "one.leaper.vibeface" or len(value["apks"]) != 1:
        raise ValueError("Android APK provenance does not bind this release source")
    entry = value["apks"][0]
    if set(entry) != {"file", "sha256", "abis", "nativeLibraries"} or not re.fullmatch(r"[A-Za-z0-9._-]+\.apk", entry["file"]) or entry["abis"] != list(ANDROID_ABIS):
        raise ValueError("Android APK provenance has unexpected file or ABI metadata")
    apk = directory / entry["file"]
    checksum = regular(apk.with_name(apk.name + ".sha256")).decode().strip().split()
    if not re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]) or checksum != [entry["sha256"], apk.name] or apk.is_symlink() or digest(apk) != entry["sha256"] or apk_native_libraries(apk) != entry["nativeLibraries"]:
        raise ValueError("Actual APK or native library bytes differ from the release artifact")
    return apk.resolve(), value


def control_descriptor(path):
    metadata = path.lstat()
    parent = path.parent.lstat()
    if path != UI / "control.json" or path.parent.is_symlink() or parent.st_uid != os.getuid() or parent.st_gid != os.getgid() or parent.st_mode & 0o777 != 0o700 or path.is_symlink() or metadata.st_uid != os.getuid() or metadata.st_gid != os.getgid() or metadata.st_mode & 0o777 != 0o600:
        raise ValueError("Read only this host user's exact private UI descriptor")
    value = json.loads(regular(path, 2048))
    url = urllib.parse.urlsplit(value.get("url", ""))
    if set(value) != {"schemaVersion", "url", "token"} or value["schemaVersion"] != 1 or not re.fullmatch(r"[a-f0-9]{64}", value.get("token", "")) or url.scheme != "http" or url.hostname != "127.0.0.1" or not url.port or url.path not in ("", "/") or url.query or url.fragment or url.username or url.password:
        raise ValueError("UI descriptor must name one authenticated literal loopback controller")
    return value


def controller(value, path, body=None):
    headers = {"Authorization": "Bearer " + value["token"]}
    if body is not None:
        headers["Content-Type"] = "application/json"
    connection = http.client.HTTPConnection("127.0.0.1", urllib.parse.urlsplit(value["url"]).port, timeout=10)
    try:
        connection.request("GET" if body is None else "POST", path, None if body is None else json.dumps(body).encode(), headers)
        response = connection.getresponse()
        if response.status != 200:
            raise ValueError("Actual loopback controller rejected its fixed operation")
        data = response.read(65537)
        if len(data) > 65536:
            raise ValueError("UI controller response exceeded its public bound")
    finally:
        connection.close()
    result = json.loads(data)
    if set(result) != {"ok", "result"} or result["ok"] is not True:
        raise ValueError("Actual UI controller rejected its bounded operation")
    return result["result"]


def wait_owned(description, deadline, predicate, processes=()):
    while time.monotonic() < deadline:
        if any(process.poll() is not None for process in processes):
            raise RuntimeError(description + " stopped before readiness")
        value = predicate()
        if value:
            return value
        time.sleep(0.25)
    raise TimeoutError(description + " exceeded its explicit runtime limit")


class AndroidHost:
    def __init__(self):
        self.process = None
        self.reverse = {}
        self.server_port = None
        self.log = None

    def adb(self, *arguments, check=True):
        result = subprocess.run([str(self.sdk / "platform-tools/adb"), "-P", str(self.server_port), *arguments], env=self.environment, capture_output=True, timeout=20)
        if check and result.returncode:
            raise RuntimeError("Task Android command failed")
        return result.stdout.decode().strip()

    def start(self):
        self.sdk = UI / "android-sdk"
        self.sdk.mkdir(parents=True, exist_ok=False)
        self.server_port = available_port()
        self.environment = {**os.environ, "ANDROID_SDK_ROOT": str(self.sdk), "ANDROID_HOME": str(self.sdk), "ANDROID_USER_HOME": str(UI / "android-user"),
            "ANDROID_EMULATOR_HOME": str(UI / "android-emulator"), "ANDROID_AVD_HOME": str(UI / "avd"),
            "ANDROID_ADB_SERVER_PORT": str(self.server_port), "ADB_SERVER_SOCKET": f"tcp:127.0.0.1:{self.server_port}",
            "AGENT_DEVICE_HOME": str(UI / "agent-device"), "AGENT_DEVICE_STATE_DIR": str(UI / "agent-device"),
            "PLAYWRIGHT_BROWSERS_PATH": str(UI / "browsers"), "E2E_TELEMETRY_DISABLED": "1"}
        manager = android_tool("sdkmanager")
        log = UI / "private-android-setup.log"
        command("install task Android runtime", [manager, "--sdk_root=" + str(self.sdk), "cmdline-tools;20.0", "platform-tools", "emulator", "system-images;android-35;default;x86_64"], log, env=self.environment, input=b"y\n" * 64, timeout=900)
        avdmanager = str(self.sdk / "cmdline-tools/20.0/bin/avdmanager")
        name = "vdp-paid-" + uuid.uuid4().hex[:12]
        command("create dedicated task AVD", [avdmanager, "create", "avd", "--name", name, "--package", "system-images;android-35;default;x86_64", "--device", "pixel_6", "--path", str(UI / "avd" / (name + ".avd"))], log, env=self.environment, input=b"no\n", timeout=120)
        self.environment["PATH"] = str(self.sdk / "platform-tools") + os.pathsep + self.environment["PATH"]
        port = next(port for port in range(5554, 5683, 2) if port_free(port) and port_free(port + 1))
        self.serial = f"emulator-{port}"
        self.adb("start-server")
        self.log = log.open("ab")
        self.process = subprocess.Popen([str(self.sdk / "emulator/emulator"), "-avd", name, "-port", str(port), "-no-window", "-no-audio", "-no-boot-anim", "-no-snapshot", "-wipe-data", "-gpu", "swiftshader_indirect", "-memory", "2048", "-cores", "2"], env=self.environment, stdout=self.log, stderr=self.log)
        wait_owned("task Android boot", time.monotonic() + 300, lambda: self.adb("-s", self.serial, "shell", "getprop", "sys.boot_completed", check=False) == "1", (self.process,))
        self.adb("-s", self.serial, "root")
        self.adb("-s", self.serial, "wait-for-device")
        if self.adb("-s", self.serial, "shell", "id", "-u") != "0" or self.adb("-s", self.serial, "shell", "getprop", "ro.product.cpu.abi") != "x86_64":
            raise ValueError("Only the task root x86_64 AVD may inspect its saved transfer")
        return self.environment

    def save_reverse(self, port):
        local = f"tcp:{port}"
        mappings = [line.split() for line in self.adb("-s", self.serial, "reverse", "--list").splitlines()]
        self.reverse[local] = next((entry[2] for entry in mappings if len(entry) == 3 and entry[1] == local), None)

    def close(self):
        failed = False
        try:
            if self.server_port is not None and hasattr(self, "serial"):
                for local, original in self.reverse.items():
                    try:
                        mappings = [line.split() for line in self.adb("-s", self.serial, "reverse", "--list").splitlines()]
                        current = {entry[1]: entry[2] for entry in mappings if len(entry) == 3}
                        if local in current and current[local] != original:
                            self.adb("-s", self.serial, "reverse", "--remove", local)
                        if original is not None and current.get(local) != original:
                            self.adb("-s", self.serial, "reverse", local, original)
                    except (OSError, RuntimeError, subprocess.TimeoutExpired):
                        failed = True
                for arguments in (("-s", self.serial, "emu", "kill"), ("kill-server",)):
                    try:
                        self.adb(*arguments, check=arguments == ("kill-server",))
                    except (OSError, RuntimeError, subprocess.TimeoutExpired):
                        failed = True
        finally:
            if self.process is not None and self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)
            if self.log is not None:
                self.log.close()
        if failed:
            raise RuntimeError("Dedicated Android cleanup did not fully confirm its owned reverse and ADB server")


def available_port():
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        return server.getsockname()[1]


def port_free(port):
    try:
        with socket.socket() as server:
            server.bind(("127.0.0.1", port))
        return True
    except OSError:
        return False


def live_container(command, package, expected, name):
    result = command[:-4]
    result.extend(["--name", name, "--network", "host", "--env", "TOKIO_WORKER_THREADS=2", "--env", f"VIBEFACE_DELIVERY_UI_UID={os.getuid()}", "--env", f"VIBEFACE_DELIVERY_UI_GID={os.getgid()}", command[-4], "python3", "scripts/run-delivery-live.py", "--inside",
                   "--package", str(Path("/work") / package.relative_to(ROOT)), "--source-commit", expected])
    return result


def terminate_owned(process):
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=5)


def join_owned(container, ui, android, deadline):
    while time.monotonic() < deadline:
        proof_status, ui_status = container.poll(), ui.poll()
        if proof_status not in (None, 0) or ui_status not in (None, 0) or android.poll() is not None:
            raise RuntimeError("A genuine proof, actual UI or dedicated Android process failed")
        if proof_status == ui_status == 0:
            return
        time.sleep(0.25)
    raise TimeoutError("Proof and UI processes did not both finish within the acceptance bound")


def remaining_ui_timeout_ms(deadline):
    remaining = int((deadline - time.monotonic()) * 1000)
    if not 0 < remaining <= LIVE_TIMEOUT_SECONDS * 1000:
        raise TimeoutError("Paid UI total timeout must remain within the original live deadline")
    return remaining


def finish_failed_ui_evidence(container, ui, descriptor, deadline):
    if container is None or ui is None or container.poll() is not None or ui.poll() in (None, 0):
        return
    until = min(deadline, time.monotonic() + 60)
    if descriptor is not None:
        try:
            controller(descriptor, "/abort", {"role": "web"})
        except (OSError, ValueError, http.client.HTTPException):
            pass
    while container.poll() is None and time.monotonic() < until:
        time.sleep(0.25)


def cleanup_owned(android, proof_process, ui_process, name, descriptor, success):
    failed = False
    if not success and descriptor is not None:
        try:
            controller(descriptor, "/abort", {"role": "web"})
        except (OSError, ValueError, http.client.HTTPException):
            pass
    try:
        terminate_owned(ui_process)
    except (OSError, subprocess.SubprocessError):
        failed = True
    if proof_process is not None:
        try:
            stopped = subprocess.run(["docker", "stop", "--time", "10", name], capture_output=True, timeout=30)
            if proof_process.poll() is None and stopped.returncode:
                subprocess.run(["docker", "rm", "--force", name], capture_output=True, timeout=20, check=True)
        except (OSError, subprocess.SubprocessError):
            failed = True
    try:
        terminate_owned(proof_process)
    except (OSError, subprocess.SubprocessError):
        failed = True
    try:
        android.close()
    except (OSError, RuntimeError, subprocess.SubprocessError):
        failed = True
    try:
        (UI / "control.json").unlink(missing_ok=True)
    except OSError:
        failed = True
    return failed


def dispatch(package_input, android_input, expected):
    source_commit(expected)
    apk, android_provenance = checked_android(android_input, expected)
    UI.mkdir(parents=True, mode=0o700, exist_ok=True)
    if UI.is_symlink() or UI.stat().st_uid != os.getuid() or UI.stat().st_gid != os.getgid():
        raise ValueError("Live UI requires this host user's dedicated task directory")
    UI.chmod(0o700)
    if any((UI / path).exists() or (UI / path).is_symlink() for path in ("control.json", "ordinary-paid-ui.bin", "android-sdk", "public", "e2e", "agent-device")):
        raise ValueError("Live acceptance requires fresh task UI, Android and driver state")
    PUBLIC.mkdir(parents=True, mode=0o755, exist_ok=False)
    PUBLIC.chmod(0o755)
    (PUBLIC / "ui").mkdir(mode=0o755)
    checks = runpy.run_path(str(ROOT / "scripts/check-delivery.py"))
    package = unpack_package(package_input)
    checked_package(package, expected)
    for name in ("cargo", "risc0", "canonical"):
        (ARTIFACTS / name).mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="live-build-context-", dir=ARTIFACTS) as directory:
        context = Path(directory)
        for name in ("proofs/delivery/Sandbox.Dockerfile", "proofs/delivery/confined-gnark.sh"):
            target = context / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, target)
        build, container = checks["sandbox_commands"](context)
        subprocess.run(build, cwd=ROOT, check=True)
    android, proof_process, ui_process, descriptor = AndroidHost(), None, None, None
    name = "vdp-live-" + uuid.uuid4().hex
    deadline = time.monotonic() + LIVE_TIMEOUT_SECONDS
    success, cleanup_failure = False, False
    previous_term = signal.getsignal(signal.SIGTERM)

    def interrupted(*_):
        raise InterruptedError("Live proof and UI dispatcher interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    try:
        environment = android.start()
        with (UI / "private-container.log").open("ab") as proof_log, (UI / "private-ui.log").open("ab") as ui_log:
            proof_process = subprocess.Popen(live_container(container, package, expected, name), cwd=ROOT,
                                             stdout=proof_log, stderr=proof_log, start_new_session=True)
            control = UI / "control.json"
            descriptor = wait_owned("early paid UI pairing controller", deadline,
                lambda: control_descriptor(control) if control.exists() else None, (proof_process, android.process))
            facts = controller(descriptor, "/facts")
            if facts.get("stage") not in ("bootstrap", "inspections"):
                raise ValueError("The first paid UI controller must expose pairing and inspections before ordinary mutations")
            receiver = urllib.parse.urlsplit(facts["nodes"]["android"]["url"])
            if receiver.scheme != "http" or receiver.hostname != "127.0.0.1" or not receiver.port or receiver.path not in ("", "/") or receiver.query or receiver.fragment or receiver.username or receiver.password:
                raise ValueError("Only the actual receiver's loopback port may receive an ADB reverse")
            android.save_reverse(receiver.port)
            environment = {**environment, "VIBEFACE_DELIVERY_UI_CONTROL": str(control), "VIBEFACE_E2E_ANDROID_APK": str(apk),
                "VIBEFACE_E2E_ANDROID_SERIAL": android.serial, "VIBEFACE_DELIVERY_UI_TIMEOUT_MS": str(UI_TIMEOUT_MS),
                "VIBEFACE_DELIVERY_UI_TOTAL_TIMEOUT_MS": str(remaining_ui_timeout_ms(deadline))}
            authority = UI / "xauthority"
            os.close(os.open(authority, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600))
            ui_process = subprocess.Popen(["xvfb-run", "--auto-servernum", "--server-num", "91", "--auth-file", str(authority),
                "--error-file", str(UI / "private-display.log"), "--server-args=-screen 0 1440x1000x24 -nolisten tcp",
                "pnpm", "exec", "e2e", "run", "--config", "e2e.paid.config.ts", "--workers", "2", "--headed",
                "--no-cache", "--retries", "0", "--video=off"], cwd=ROOT, env=environment, stdout=ui_log, stderr=ui_log, start_new_session=True)
            join_owned(proof_process, ui_process, android.process, deadline)
            finalize_ui(android_provenance)
            success = True
    except BaseException:
        finish_failed_ui_evidence(proof_process, ui_process, descriptor, deadline)
        raise
    finally:
        try:
            cleanup_failure = cleanup_owned(android, proof_process, ui_process, name, descriptor, success)
        finally:
            signal.signal(signal.SIGTERM, previous_term)
        if not success or cleanup_failure:
            mark_host_failure(expected, "owned_cleanup_failed" if cleanup_failure else "live_ui_or_proof_failed")
        if cleanup_failure:
            raise RuntimeError("Live acceptance failed to confirm cleanup of its owned processes")
    print("PASS delivery-live thirteen genuine proofs and actual Web and Android paid delivery", flush=True)


def resources():
    membership = next(line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines() if line.startswith("0::"))
    cgroup = Path("/sys/fs/cgroup") / membership.lstrip("/")
    fields = {name: (cgroup / name).read_text().strip() for name in (
        "memory.max", "memory.swap.max", "memory.current", "memory.events", "pids.max", "pids.current")}
    if fields["memory.max"] != str(11 * 1024 ** 3) or fields["memory.swap.max"] != "0" or fields["pids.max"] != "64":
        raise ValueError("Live proof sandbox has incorrect kernel memory, swap or process limits")
    for name in ("memory.peak", "pids.peak"):
        if (cgroup / name).is_file():
            fields[name] = (cgroup / name).read_text().strip()
    return cgroup, fields


def oom_delta(initial, final):
    def counts(snapshot):
        values = {}
        for line in snapshot["memory.events"].splitlines():
            name, value = line.split()
            if name in values or not re.fullmatch(r"[0-9]+", value):
                raise ValueError("Invalid cgroup memory event counter")
            values[name] = int(value)
        if not {"oom", "oom_kill"}.issubset(values):
            raise ValueError("Required cgroup OOM counters are missing")
        return {name: values.get(name, 0) for name in ("oom", "oom_kill", "oom_group_kill")}
    before, after = counts(initial), counts(final)
    delta = {name: after[name] - before[name] for name in before}
    if any(value < 0 for value in delta.values()):
        raise ValueError("Cgroup OOM counters decreased during live acceptance")
    return delta


def command(label, arguments, log, **kwargs):
    print("RUN delivery-live " + label, flush=True)
    with log.open("ab") as output:
        result = subprocess.run(arguments, cwd=ROOT, stdout=output, stderr=output, **kwargs)
    if result.returncode:
        raise RuntimeError(f"{label} failed with exit {result.returncode}; private task log withheld")


def glibc_failure(directory, log=None):
    files = [*directory.rglob("process.log"), *([log] if log is not None else [])]
    return any(re.search(rb"(?:version [`']GLIBC_[0-9.]+['`] not found|symbol lookup error:.*GLIBC_[0-9.]+)", file.read_bytes())
               for file in files if file.is_file() and not file.is_symlink())


def runtime(binary, module, destination, log, environment):
    try:
        command("packaged Node real Web process", ["python3", "scripts/verify-node.py", "--web-only", "--binary", str(binary),
                "--crypto-module", str(module), "--artifacts", str(destination)], log, env=environment)
        return True
    except RuntimeError:
        if not glibc_failure(destination, log):
            raise
        return False


def publish_file(source, destination, limit=16384):
    regular(source, limit)
    destination.parent.mkdir(parents=True, mode=0o755, exist_ok=True)
    destination.parent.chmod(0o755)
    shutil.copyfile(source, destination)
    destination.chmod(0o644)


def replace_public(file, value):
    temporary = file.with_name("." + file.name + "." + uuid.uuid4().hex)
    write_private(temporary, value)
    temporary.chmod(0o644)
    os.replace(temporary, file)


def public_proof(directory, name, relation):
    value = json.loads(regular(directory / (name + ".json"), 16384))
    if set(value) != {"mode", "kind", "relation", "imageId", "profileId", "journal", "journalHash", "seal"} or value["mode"] != "prove" or value["kind"] != "groth16" or type(value["relation"]) is not int or value["relation"] != relation:
        raise ValueError("Only the exact genuine public proof schema may be exported")
    if any(not isinstance(value[field], str) or not re.fullmatch(r"[0-9a-f]{64}", value[field]) for field in ("imageId", "profileId", "journalHash")):
        raise ValueError("Public proof hashes must be canonical SHA-256 values")
    selector = re.search(r"bytes4 public constant SELECTOR\s*=\s*0x([0-9a-f]{8});", regular(ROOT / "contracts/delivery/RiscZeroAdapter.sol").decode())
    if selector is None or not re.fullmatch(r"[0-9a-f]{3008}", value["journal"]) or not re.fullmatch(r"[0-9a-f]{520}", value["seal"]) or not value["seal"].startswith(selector.group(1)):
        raise ValueError("Public proof journal or EVM seal is invalid")
    journal = bytes.fromhex(value["journal"])
    if int.from_bytes(journal[32:64], "big") != relation or hashlib.sha256(journal).hexdigest() != value["journalHash"] or journal[128:160].hex() != value["profileId"] or journal[160:192].hex() != value["imageId"]:
        raise ValueError("Public proof metadata does not bind its actual journal bytes")
    regular(directory / (name + ".receipt"), 16384)
    return {"relation": relation, "imageId": value["imageId"], "profileId": value["profileId"],
            "journalHash": value["journalHash"], "receiptSha256": digest(directory / (name + ".receipt"))}


def settlement_export(run, directory, windows, amount, *, destination=None):
    source = run / directory
    bundle = source / "settlement.json"
    value = json.loads(regular(bundle))
    if set(value) != {"schemaVersion", "escrow", "asset", "orderId", "proofs", "capsules", "transactions"} or value["schemaVersion"] != 1:
        raise ValueError("Settlement export contains unexpected private fields")
    if any(not re.fullmatch(r"0x[0-9a-fA-F]{40}", value[field]) for field in ("escrow", "asset")) or not re.fullmatch(r"0x[0-9a-f]{64}", value["orderId"]):
        raise ValueError("Settlement export contains invalid public chain identities")
    expected = [{"relation": 1, "public": "key-binding.json", "receipt": "key-binding.receipt"}]
    expected.extend({"relation": relation, "windowIndex": index, "public": f"relation-{relation}-window-{index}.json",
                     "receipt": f"relation-{relation}-window-{index}.receipt"} for index in range(windows) for relation in (2, 3, 4))
    if value["proofs"] != expected:
        raise ValueError("Settlement must export exactly the identified public proofs in canonical order")
    if not isinstance(value["capsules"], list) or len(value["capsules"]) != windows or any(set(item) != {"windowIndex", "capsule"} or type(item["windowIndex"]) is not int or item["windowIndex"] != index or not re.fullmatch(r"[0-9a-f]{160}", item["capsule"]) for index, item in enumerate(value["capsules"])):
        raise ValueError("Settlement exports only the exact public encrypted capsules")
    transactions = value["transactions"]
    if set(transactions) not in ({"registerKey", "fund", "hold", "publish", "open", "withdraw"}, {"registerKey", "approve", "fund", "hold", "publish", "open", "withdraw"}):
        raise ValueError("Settlement transaction export contains private or unexpected fields")
    hashes = [transactions["registerKey"], transactions["fund"], *([transactions["approve"]] if "approve" in transactions else [])]
    for action in ("hold", "publish", "open", "withdraw"):
        if not isinstance(transactions[action], list) or not 1 <= len(transactions[action]) <= windows or (action != "withdraw" and len(transactions[action]) != windows):
            raise ValueError("Settlement lacks its actual per-window transaction hashes")
        hashes.extend(transactions[action])
    if any(not isinstance(value, str) or not re.fullmatch(r"0x[0-9a-f]{64}", value) for value in hashes) or len(set(hashes)) != len(hashes):
        raise ValueError("Settlement contains invalid or duplicate transaction hashes")
    proofs = [public_proof(source, Path(entry["public"]).stem, entry["relation"]) for entry in expected]
    observation = source / "settlement-observation.json"
    observed = json.loads(regular(observation, 65536))
    observer_fields = {"schemaVersion", "status", "scope", "checks", "transactions", "proofs", "orderId", "escrow", "asset", "imageId", "profileId", "confirmationPolicy", "adapter", "verifier", "totalPlaintextBytes", "windows", "money", "gas", "case"}
    if not set(observed).issubset(observer_fields):
        raise ValueError("Public observer output contains unexpected fields")
    if observed.get("status") != "PASS" or observed.get("orderId") != value["orderId"] or any(observed.get("money", {}).get(name) != amount for name in ("funded", "earned", "withdrawn")) or observed.get("money", {}).get("refunded") != "0":
        raise ValueError("This bundle's independent observer did not confirm its exact actual payment")
    observed_proofs = observed.get("proofs", [])
    if len(observed_proofs) != len(proofs) or {(entry.get("journalHash"), entry.get("receiptSha256")) for entry in observed_proofs} != {("0x" + entry["journalHash"], "0x" + entry["receiptSha256"]) for entry in proofs}:
        raise ValueError("The independent observer did not inspect these exact public receipts")
    destination = destination if destination is not None else (PUBLIC if directory == "settlement" else PUBLIC / directory)
    for entry in expected:
        for extension in (".json", ".receipt"):
            name = Path(entry["public"]).stem + extension
            publish_file(source / name, destination / name)
    publish_file(bundle, destination / "settlement.json", 65536)
    publish_file(observation, destination / "settlement-observation.json", 65536)
    return value, proofs


def preserve_strict_evidence(run, compiled, profile, prover):
    results = list((run / "process").glob("delivery-paid-*/summary.json"))
    if len(results) != 1:
        return None
    regular(results[0])
    source = results[0].parent / "settlement"
    encoded = hashlib.sha256(b"vibeface-delivery-profile-v1").digest() + bytes(profile["guest_image_id"]) + bytes(profile["statement_schema_hash"])
    for field in ("crypto_suite", "max_windows", "max_window_bytes", "max_total_bytes", "max_guest_cycles", "max_prover_memory_bytes"):
        encoded += profile[field].to_bytes(32, "big")
    profile_id = hashlib.sha256(encoded).hexdigest()
    for name in PROOFS:
        relation = 1 if name == "key-binding" else int(name.split("-")[1])
        fact = public_proof(source, name, relation)
        metadata = json.loads(regular(source / (name + ".json"), 16384))
        if fact["imageId"] != compiled["imageId"] or fact["profileId"] != profile_id:
            raise ValueError("Partial strict evidence belongs to another current image or profile")
        verified = subprocess.run([str(prover), "verify", metadata["journal"]], input=regular(source / (name + ".receipt"), 16384),
            capture_output=True, timeout=120, env={"PATH": "/usr/bin:/bin", "RISC0_SERVER_PATH": "/opt/vdp/bin/r0vm"})
        if verified.returncode != 0 or len(verified.stdout) > 16384:
            raise ValueError("Partial strict receipt did not pass the actual current host verifier")
        value = json.loads(verified.stdout)
        if value.get("verified") is not True or any(value.get(field) != expected for field, expected in (("kind", "groth16"), ("imageId", compiled["imageId"]), ("journalHash", fact["journalHash"]), ("seal", metadata["seal"]))):
            raise ValueError("Partial strict host verification did not bind the actual public receipt and seal")
    names = [*(name + extension for name in PROOFS for extension in (".json", ".receipt")), "settlement.json", "settlement-observation.json"]
    if any((PUBLIC / name).exists() or (PUBLIC / name).is_symlink() for name in names):
        raise ValueError("Partial evidence must not replace any existing public result")
    moved = []
    try:
        with tempfile.TemporaryDirectory(prefix="strict-public-", dir=run) as staging:
            settlement_export(results[0].parent, "settlement", 2, "29", destination=Path(staging))
            for name in names:
                os.replace(Path(staging) / name, PUBLIC / name)
                moved.append(PUBLIC / name)
    except BaseException:
        for file in moved:
            file.unlink(missing_ok=True)
        raise
    return {"scope": "strict-only", "genuineProofs": 7, "imageId": compiled["imageId"], "profileId": profile_id,
            "hostVerified": True, "observerScope": "recorded strict observation before later failure",
            "canonicalRestorationVerified": False}


def checked_ordinary_case(report):
    case = report.get("cases", {}).get("ordinary-paid-real-web-android-offline-recovery-and-save")
    if report.get("status") != "passed" or not isinstance(case, dict):
        raise ValueError("Actual Web and Android delivery requires the successful ordinary paid case")
    for name, expected in (("bytes", 4096), ("windows", 1), ("new_proofs", 3), ("public_proofs", 4)):
        if type(case.get(name)) is not int or case[name] != expected:
            raise ValueError("Ordinary paid delivery has incorrect capacity or genuine proof evidence")
    if case.get("acceptance") != "recoverable" or any(case.get(name) != value for name, value in (("earned", "17"), ("withdrawn", "17"), ("refunded", "0"))):
        raise ValueError("Ordinary paid delivery must independently settle the exact 17 TestToken")
    if any(case.get(name) is not True for name in ("actual_web_confirmation", "actual_android_acceptance", "actual_android_saved_file", "offline_visible", "settled_while_app_background", "read_only_observer")):
        raise ValueError("Ordinary delivery lacks an actual rendered UI, offline, saved-file or observer result")
    if any(not isinstance(case.get(field), str) or not re.fullmatch(r"[0-9a-f]{64}", case[field]) for field in ("order_id", "sha256")) or str(uuid.UUID(case["intent_id"])) != case["intent_id"]:
        raise ValueError("Ordinary delivery has invalid original file or durable intent identities")
    if not re.fullmatch(r"[A-Za-z0-9/_-]{1,160}", case.get("current_registration", "")):
        raise ValueError("Ordinary delivery must name its actual active key registration")
    events = case.get("ui_events", {})
    if list(events) != list(UI_EVENTS):
        raise ValueError("Actual UI delivery did not complete every required ordered stage")
    previous = 0
    for event in events.values():
        if set(event) != {"order_id", "intent_id", "monotonic"} or event["order_id"] != case["order_id"] or event["intent_id"] != case["intent_id"] or type(event["monotonic"]) not in (int, float) or not math.isfinite(event["monotonic"]) or event["monotonic"] <= previous:
            raise ValueError("Actual UI stages must reference the same order and intent in monotonic sequence")
        previous = event["monotonic"]
    return {"orderId": case["order_id"], "intentId": case["intent_id"], "bytes": 4096, "sha256": case["sha256"],
            "acceptance": "recoverable", "newProofs": 3, "publicProofs": 4, "currentRegistrationId": case["current_registration"],
            "earned": "17", "withdrawn": "17", "refunded": "0", "uiEvents": events, "readOnlyObserver": True}


def lifecycle_export(run, original):
    source = run / "key-lifecycle"
    value = json.loads(regular(source / "summary.json", 65536))
    required = {"schemaVersion", "kind", "status", "generatedProofs", "operations", "finalRegistrationId", "finalActiveSerial", "finalBindingJournalHash", "imageId", "profileId", "oldOrderId", "oldFileSha256", "mixedHeldObservation"}
    if set(value) != required or value["schemaVersion"] != 1 or value["kind"] != "genuine-key-lifecycle" or value["status"] != "PASS" or type(value["generatedProofs"]) is not int or value["generatedProofs"] != 3 or not isinstance(value["operations"], list) or len(value["operations"]) != 3:
        raise ValueError("Key lifecycle must report its three actual confirmed genuine proofs")
    if value["oldFileSha256"] != original["sha256"] or not re.fullmatch(r"0x[0-9a-f]{64}", value["oldOrderId"]) or value["mixedHeldObservation"] != "mixed-held.json":
        raise ValueError("Key lifecycle must retain the actual strict old order and original file")
    fields = {"action", "bindingAction", "serial", "intentId", "bindingHash", "transactionId", "transactionHash", "relation", "public", "receipt", "journalHash", "receiptSha256", "newProofs", "registryConfirmed", "retryReusedIntent", "restartedWalletLocked", "oldOrderReadSha256"}
    proofs, serials, transactions = [], set(), set()
    for operation, action in zip(value["operations"], ("rotate", "revoke", "register")):
        if set(operation) != fields or operation["action"] != action or operation["bindingAction"] != ("revoked" if action == "revoke" else "active") or type(operation["relation"]) is not int or operation["relation"] != 1 or type(operation["newProofs"]) is not int or operation["newProofs"] != 1 or operation["public"] != action + ".json" or operation["receipt"] != action + ".receipt":
            raise ValueError("Key lifecycle public operations have an invalid action, relation or receipt")
        if type(operation["serial"]) is not int or not 0 < operation["serial"] < 2 ** 64 or operation["serial"] in serials or not re.fullmatch(r"0x[0-9a-f]{64}", operation["transactionHash"]) or operation["transactionHash"] in transactions:
            raise ValueError("Key lifecycle requires distinct actual serials and transaction hashes")
        serials.add(operation["serial"])
        transactions.add(operation["transactionHash"])
        if str(uuid.UUID(operation["intentId"])) != operation["intentId"] or not re.fullmatch(r"[A-Za-z0-9/_-]{1,160}", operation["transactionId"]) or not re.fullmatch(r"[0-9a-f]{64}", operation["bindingHash"]):
            raise ValueError("Key lifecycle must name its actual durable intent and public binding")
        if any(operation[field] is not True for field in ("registryConfirmed", "retryReusedIntent", "restartedWalletLocked")) or operation["oldOrderReadSha256"] != original["sha256"]:
            raise ValueError("Key lifecycle lacks confirmed registry, idempotent retry, restart lock or old-file evidence")
        proof = public_proof(source, action, 1)
        journal = bytes.fromhex(json.loads(regular(source / (action + ".json"), 16384))["journal"])
        if int.from_bytes(journal[19 * 32:20 * 32], "big") != operation["serial"] or journal[22 * 32:23 * 32].hex() != operation["bindingHash"] or int.from_bytes(journal[46 * 32:47 * 32], "big") != (2 if action == "revoke" else 1):
            raise ValueError("Key lifecycle serial, binding hash and action must match the actual proof journal")
        if any(operation[field] != proof[field] for field in ("journalHash", "receiptSha256")) or any(value[field] != proof[field] for field in ("imageId", "profileId")):
            raise ValueError("Key lifecycle summary does not bind the actual public journal and receipt bytes")
        proofs.append(proof)
        for extension in (".json", ".receipt"):
            publish_file(source / (action + extension), PUBLIC / "key-lifecycle" / (action + extension))
    final = value["operations"][-1]
    if value["finalRegistrationId"] != final["transactionId"] or value["finalActiveSerial"] != final["serial"] or value["finalBindingJournalHash"] != final["journalHash"]:
        raise ValueError("Final active key must be the actual confirmed final register operation")
    mixed_held_export(source, original, value)
    publish_file(source / "summary.json", PUBLIC / "key-lifecycle/summary.json", 65536)
    return value, proofs


def mixed_held_export(source, original, lifecycle):
    value = json.loads(regular(source / "mixed-held.json", 65536))
    fields = {"schemaVersion", "sessionId", "case", "status", "helperNewProofs", "newOwnerKeyBindings", "state", "txHash", "nodeObservations", "transactions", "oldFileSha256", "imageId", "profileId", "restoreRequired", "nodeAutonomy"}
    if set(value) != fields or value["schemaVersion"] != 1 or value["case"] != "mixed-held-rotate" or value["status"] != "EVM_PASS" or type(value["helperNewProofs"]) is not int or value["helperNewProofs"] != 0 or type(value["newOwnerKeyBindings"]) is not int or value["newOwnerKeyBindings"] != 1 or value["restoreRequired"] is not True or value["nodeAutonomy"] != "REAL_OWNER_ROTATE_ONLY_KEEPER_OLD_WINDOW_SETTLEMENT":
        raise ValueError("Mixed-held evidence must describe its actual bounded historical branch")
    if value["oldFileSha256"] != original["sha256"] or value["txHash"] != lifecycle["operations"][0]["transactionHash"] or any(value[field] != "0x" + lifecycle[field] for field in ("imageId", "profileId")):
        raise ValueError("Mixed-held branch must bind the same actual old file and product proof profile")
    order, anchor = value["state"]["order"], value["state"]["anchor"]
    if any(order.get(field) != expected for field, expected in (("earned", "12"), ("withdrawn", "12"), ("refunded", "17"))) or order.get("bonusEarned") is not False or order.get("bonusRefunded") is not True or len(order.get("windows", [])) != 2:
        raise ValueError("Mixed-held branch must actually settle one held window and refund its unheld remainder")
    windows = order["windows"]
    if any(type(window.get("phase")) is not int or window["phase"] != phase or window.get("decryptionProven") is not proven for window, phase, proven in zip(windows, (3, 4), (True, False))):
        raise ValueError("Mixed-held branch lacks its actual proved and refunded window states")
    observations = []
    for item in value["nodeObservations"]:
        if set(item) != {"checkpointId", "roles", "status", "rawBroadcastDelta", "nativeRefund", "keyRotation"} or not isinstance(item["checkpointId"], str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,160}", item["checkpointId"]) or not isinstance(item["roles"], list) or any(role not in ("S", "R", "P") for role in item["roles"]) or item["status"] not in ("observed", "not_observed") or type(item["rawBroadcastDelta"]) is not int or item["rawBroadcastDelta"] < 0 or any(type(item[field]) is not bool for field in ("nativeRefund", "keyRotation")):
            raise ValueError("Mixed-held branch contains unconfirmed Node observations or unexpected private fields")
        observations.append(item)
    if not any(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*-mixed-held-result", item["checkpointId"]) and "R" in item["roles"] and item["status"] == "observed" and item["rawBroadcastDelta"] == 0 and item["nativeRefund"] is False and item["keyRotation"] is False for item in observations):
        raise ValueError("Mixed-held branch needs its real receiver's read-only projection")
    transactions = value["transactions"]
    for item in transactions:
        if set(item) != {"label", "hash", "status", "blockNumber", "blockHash", "gasUsed", "executionGasFeeWei"} or not isinstance(item["label"], str) or not re.fullmatch(r"[A-Za-z0-9/_-]{1,160}", item["label"]) or type(item["status"]) is not int or item["status"] not in (0, 1) or type(item["blockNumber"]) is not int or item["blockNumber"] < 0 or any(not re.fullmatch(r"0x[0-9a-f]{64}", item[field]) for field in ("hash", "blockHash")) or any(not re.fullmatch(r"[0-9]+", item[field]) for field in ("gasUsed", "executionGasFeeWei")):
            raise ValueError("Mixed-held branch exports only bounded public transaction evidence")
    if not isinstance(value["sessionId"], str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,160}", value["sessionId"]) or not re.fullmatch(r"0x[0-9a-f]{64}", value["txHash"]) or set(anchor) != {"number", "hash", "timestamp"} or not re.fullmatch(r"0x[0-9a-f]{64}", anchor["hash"]) or any(type(anchor[field]) is not int or anchor[field] < 0 for field in ("number", "timestamp")):
        raise ValueError("Mixed-held branch must identify its actual public historical anchor")
    checked_branch_state(value["state"])
    publish_file(source / "mixed-held.json", PUBLIC / "key-lifecycle/mixed-held.json", 65536)


def checked_branch_state(value):
    if set(value) != {"anchor", "order", "tokenBalances", "tokenMode", "tokenDecimals"}:
        raise ValueError("Branch state may export only the fixed public ledger snapshot")
    anchor = value["anchor"]
    if set(anchor) != {"number", "hash", "timestamp"} or not re.fullmatch(r"0x[0-9a-f]{64}", anchor["hash"]) or any(type(anchor[field]) is not int or anchor[field] < 0 for field in ("number", "timestamp")):
        raise ValueError("Branch state requires an actual public block anchor")
    if any(not re.fullmatch(r"0x[0-9a-fA-F]{40}", address) or not re.fullmatch(r"[0-9]+", amount) for address, amount in value["tokenBalances"].items()) or any(type(value[field]) is not int or value[field] < 0 for field in ("tokenMode", "tokenDecimals")):
        raise ValueError("Branch state contains invalid public token balances or configuration")
    order = value["order"]
    if order is None:
        return {"orderPresent": False, "phases": None, "earned": "0", "withdrawn": "0", "refunded": "0"}
    if set(order) != {"earned", "withdrawn", "refunded", "bonusEarned", "bonusRefunded", "windows"} or any(not re.fullmatch(r"[0-9]+", order[field]) for field in ("earned", "withdrawn", "refunded")) or any(type(order[field]) is not bool for field in ("bonusEarned", "bonusRefunded")) or len(order["windows"]) != 2:
        raise ValueError("Branch order must expose only its actual strict two-window ledger")
    for window in order["windows"]:
        if set(window) != {"phase", "decryptionProven", "capsule", "capsuleHash"} or type(window["phase"]) is not int or window["phase"] not in range(5) or type(window["decryptionProven"]) is not bool or not re.fullmatch(r"0x(?:[0-9a-f]{160})?", window["capsule"]) or not re.fullmatch(r"0x[0-9a-f]{64}", window["capsuleHash"]):
            raise ValueError("Branch window public state has an invalid phase, capsule or decryption flag")
        if window["capsule"] != "0x" and hashlib.sha256(bytes.fromhex(window["capsule"][2:])).hexdigest() != window["capsuleHash"][2:]:
            raise ValueError("Branch window capsule differs from its actual public commitment")
    return {"orderPresent": True, "phases": [window["phase"] for window in order["windows"]],
            **{field: order[field] for field in ("earned", "withdrawn", "refunded")}}


def checked_confirmation(value):
    if set(value) != {"kind", "confirmations", "observedTip", "anchorNumber", "anchorHash", "absoluteFinality"} or value["kind"] != "experimental-block-depth" or type(value["confirmations"]) is not int or value["confirmations"] != 2 or value["absoluteFinality"] is not False or any(type(value[field]) is not int or value[field] < 0 for field in ("observedTip", "anchorNumber")) or value["observedTip"] < value["anchorNumber"] + 1 or not re.fullmatch(r"0x[0-9a-f]{64}", value["anchorHash"]):
        raise ValueError("Branch observations must retain the actual two-confirmation experimental policy")


def checked_branch_nodes(value):
    for item in value:
        if set(item) != {"checkpointId", "roles", "status", "rawBroadcastDelta", "nativeRefund", "keyRotation"} or not isinstance(item["checkpointId"], str) or not 0 < len(item["checkpointId"]) <= 160 or not isinstance(item["roles"], list) or any(role not in ("S", "R", "P") for role in item["roles"]) or item["status"] not in ("observed", "not_observed") or type(item["rawBroadcastDelta"]) is not int or item["rawBroadcastDelta"] < 0 or any(type(item[field]) is not bool for field in ("nativeRefund", "keyRotation")):
            raise ValueError("Branch observations contain unexpected private fields or invalid Node facts")


def checked_branch_transactions(value):
    for item in value:
        if set(item) != {"label", "hash", "status", "blockNumber", "blockHash", "gasUsed", "executionGasFeeWei"} or not isinstance(item["label"], str) or not 0 < len(item["label"]) <= 160 or type(item["status"]) is not int or item["status"] not in (0, 1) or type(item["blockNumber"]) is not int or item["blockNumber"] < 0 or any(not re.fullmatch(r"0x[0-9a-f]{64}", item[field]) for field in ("hash", "blockHash")) or any(not re.fullmatch(r"[0-9]+", item[field]) for field in ("gasUsed", "executionGasFeeWei")):
            raise ValueError("Branch transactions may contain only the fixed public receipt facts")


def branch_exports(report, lifecycle, key_directory, original):
    case = report.get("cases", {}).get("genuine-branches-and-canonical-restoration", {})
    if case.get("status") != "passed" or type(case.get("helper_new_proofs")) is not int or case["helper_new_proofs"] != 0 or case.get("core_restored") is not True or not isinstance(case.get("session_id"), str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,160}", case["session_id"]):
        raise ValueError("Full paid acceptance requires this actual completed branch and restoration session")
    session = case["session_id"]
    index_path = Path(".artifacts/vdp/contracts/branch-observations") / session / "index.json"
    if case.get("public_index") != index_path.as_posix():
        raise ValueError("Read the exact current session's public branch index; never a latest glob")
    directory = ROOT / index_path.parent
    if directory.is_symlink() or directory.resolve() != directory:
        raise ValueError("Public branch records must remain in their literal task directory")
    index = json.loads(regular(ROOT / index_path, 256 * 1024))
    fields = {"schemaVersion", "sessionId", "kind", "imageId", "profileId", "requiredCases", "coreRestored", "restore", "cases", "lifecycle", "absoluteFinality"}
    if set(index) != fields or index["schemaVersion"] != 1 or index["sessionId"] != session or index["kind"] != "genuine-settlement-branch-index" or index["coreRestored"] is not True or index["absoluteFinality"] is not False or len(index["requiredCases"]) != len(BRANCH_CASES) or set(index["requiredCases"]) != set(BRANCH_CASES) or len(index["cases"]) != len(BRANCH_CASES):
        raise ValueError("The branch index must include every finite case and actual canonical restoration")
    if any(index[field] != "0x" + lifecycle[field] for field in ("imageId", "profileId")):
        raise ValueError("The complete branch index must bind the same actual product proof image and profile")
    entries, sources = {}, []
    ledger_fields = {"orderPresent", "phases", "earned", "withdrawn", "refunded"}
    normal_fields = {"schemaVersion", "sessionId", "case", "status", "newProofs", "originalGenuineProofSet", "state", "imageId", "profileId", "confirmationPolicy", "transactions", "nodeObservations", "nodeAutonomy"}
    mixed_fields = {"schemaVersion", "sessionId", "case", "status", "helperNewProofs", "newOwnerKeyBindings", "state", "txHash", "nodeObservations", "transactions", "oldFileSha256", "imageId", "profileId", "restoreRequired", "nodeAutonomy"}
    for entry in index["cases"]:
        if set(entry) != {"case", "status", "file", "fileSha256", "expected", "actual", "nodeObservation", "nodeAutonomy"} or entry["case"] not in BRANCH_CASES or entry["case"] in entries or entry["status"] != "EVM_PASS" or entry["file"] != "branch-" + entry["case"] + ".json" or not re.fullmatch(r"[0-9a-f]{64}", entry["fileSha256"]):
            raise ValueError("Each required branch needs its distinct actual public success receipt")
        file = directory / entry["file"]
        record = json.loads(regular(file, 256 * 1024))
        if digest(file) != entry["fileSha256"] or any(record.get(field) != expected for field, expected in (("schemaVersion", 1), ("sessionId", session), ("case", entry["case"]), ("status", "EVM_PASS"), ("imageId", index["imageId"]), ("profileId", index["profileId"]), ("nodeAutonomy", entry["nodeAutonomy"]))):
            raise ValueError("Branch index does not bind the actual named public record bytes")
        if not isinstance(record["nodeAutonomy"], str) or not 0 < len(record["nodeAutonomy"]) <= 256:
            raise ValueError("Each branch must keep its actual Node autonomy scope explicit")
        incoming = entry["case"].startswith("incoming-")
        if entry["case"] == "mixed-held-rotate":
            if set(record) != mixed_fields or regular(file, 256 * 1024) != regular(key_directory / "mixed-held.json", 256 * 1024):
                raise ValueError("Lifecycle mixed-held evidence must be this exact session's actual branch receipt")
        else:
            if set(record) != normal_fields | ({"snapshotKind", "actualFundReceiptStatus"} if incoming else set()) or type(record["newProofs"]) is not int or record["newProofs"] != 0 or type(record["originalGenuineProofSet"]) is not int or record["originalGenuineProofSet"] != 7:
                raise ValueError("Branch execution must reuse the original seven genuine seals without new helper proofs")
            checked_confirmation(record["confirmationPolicy"])
        actual = checked_branch_state(record["state"])
        if incoming:
            if record["snapshotKind"] not in ("C0Prime", "C0") or type(record["actualFundReceiptStatus"]) is not int or record["actualFundReceiptStatus"] not in (0, 1) or record["nodeAutonomy"] != "NOT_TESTED_ORIGINAL_BROADCAST_RAW_ONLY":
                raise ValueError("Incoming token branches must identify actual original-raw funding and its limited Node scope")
            actual["fundReceiptStatus"] = record["actualFundReceiptStatus"]
            successful = entry["case"] in ("incoming-reentrancy", "incoming-nonreturn")
            if actual["orderPresent"] is not successful or actual["fundReceiptStatus"] != int(successful):
                raise ValueError("Incoming exact-transfer behavior differs from its real funding result")
        expected_fields = ledger_fields | ({"fundReceiptStatus"} if incoming else set())
        if not isinstance(entry["expected"], dict) or set(entry["expected"]) != expected_fields or not isinstance(entry["actual"], dict) or set(entry["actual"]) != expected_fields or entry["actual"] != actual or entry["expected"] != actual:
            raise ValueError("Every branch index verdict must equal the actual public ledger and its specified outcome")
        checked_branch_nodes(record["nodeObservations"])
        checked_branch_transactions(record["transactions"])
        observation = entry["nodeObservation"]
        observed_roles = {role for item in record["nodeObservations"] if item["status"] == "observed" for role in item["roles"]}
        if set(observation) != {"status", "roles"} or observation["status"] not in ("OBSERVED", "NOT_OBSERVED") or not isinstance(observation["roles"], list) or not set(observation["roles"]).issubset({"S", "R", "P"}) or (observation["status"] == "OBSERVED" and (not observation["roles"] or not set(observation["roles"]).issubset(observed_roles))):
            raise ValueError("Branch Node observation summary must match its actual bounded checkpoints")
        entries[entry["case"]] = entry
        sources.append(file)
    if len(index["lifecycle"]) != 3:
        raise ValueError("The branch index requires all three actual key lifecycle observations")
    for entry, operation in zip(index["lifecycle"], lifecycle["operations"]):
        action = operation["action"]
        if set(entry) != {"case", "status", "file", "txHash", "fileSha256"} or entry["case"] != action or entry["status"] != "PASS" or entry["file"] != "key-" + action + ".json" or entry["txHash"] != operation["transactionHash"]:
            raise ValueError("Branch lifecycle index must reference the actual owner key transaction")
        file = directory / entry["file"]
        record = json.loads(regular(file, 65536))
        key_fields = {"schemaVersion", "sessionId", "case", "status", "serial", "bindingAction", "bindingHash", "txHash", "registryConfirmed", "helperNewProofs", "proofGenerationOwner", "imageId", "profileId", "proof", "blockNumber", "blockHash", "gasUsed", "executionGasFeeWei", "absoluteFinality", "oldOrderReadSha256", "mixedHeld"}
        if set(record) != key_fields or digest(file) != entry["fileSha256"] or any(record[field] != expected for field, expected in (("schemaVersion", 1), ("sessionId", session), ("case", action), ("status", "PASS"), ("serial", operation["serial"]), ("bindingAction", 2 if action == "revoke" else 1), ("bindingHash", "0x" + operation["bindingHash"]), ("txHash", operation["transactionHash"]), ("imageId", index["imageId"]), ("profileId", index["profileId"]), ("proofGenerationOwner", "acceptance"), ("oldOrderReadSha256", original["sha256"]), ("mixedHeld", "PASS" if action == "rotate" else "NOT_TESTED"))) or record["registryConfirmed"] is not True or type(record["helperNewProofs"]) is not int or record["helperNewProofs"] != 0 or record["absoluteFinality"] is not False:
            raise ValueError("Key branch observation does not match the actual owner operation, journal or original-file read")
        metadata = json.loads(regular(key_directory / (action + ".json"), 16384))
        if record["proof"] != {"relation": 1, "journal": metadata["journal"], "journalHash": metadata["journalHash"], "seal": metadata["seal"], "verification": "production-adapter"} or type(record["blockNumber"]) is not int or record["blockNumber"] < 0 or not re.fullmatch(r"0x[0-9a-f]{64}", record["blockHash"]) or any(not re.fullmatch(r"[0-9]+", record[field]) for field in ("gasUsed", "executionGasFeeWei")):
            raise ValueError("Key branch must verify the exact actual production seal and retain public chain receipt facts")
        sources.append(file)
    restore = index["restore"]
    if set(restore) != {"status", "file", "fileSha256", "confirmationAnchor"} or restore["status"] != "PASS" or restore["file"] != "core-restored.json":
        raise ValueError("Complete branch acceptance requires a fresh successful core restoration receipt")
    file = directory / "core-restored.json"
    core = json.loads(regular(file, 256 * 1024))
    core_fields = {"schemaVersion", "sessionId", "event", "id", "status", "scope", "originalHashesAndNonces", "restoredAuthorizedRotation", "branches", "imageId", "profileId", "confirmationPolicy", "state", "oldFileSha256", "absoluteFinality"}
    if set(core) != core_fields or digest(file) != restore["fileSha256"] or any(core[field] != expected for field, expected in (("schemaVersion", 1), ("sessionId", session), ("event", "core-restored"), ("status", "PASS"), ("scope", "original strict core only"), ("imageId", index["imageId"]), ("profileId", index["profileId"]), ("oldFileSha256", original["sha256"]))) or core["originalHashesAndNonces"] is not True or core["absoluteFinality"] is not False or core["confirmationPolicy"] != restore["confirmationAnchor"]:
        raise ValueError("Canonical restoration must bind its actual original transactions, file and independent observer anchor")
    checked_confirmation(core["confirmationPolicy"])
    if checked_branch_state(core["state"]) != {"orderPresent": True, "phases": [3, 3], "earned": "29", "withdrawn": "29", "refunded": "0"} or any(window["decryptionProven"] is not True for window in core["state"]["order"]["windows"]):
        raise ValueError("Canonical restoration did not recover the actual completed strict order and 29 TestToken payment")
    expected_branches = {entry["case"]: {"case": entry["case"], "status": entry["status"], "nodeAutonomy": entry["nodeAutonomy"]} for entry in index["cases"]}
    if len(core["branches"]) != len(BRANCH_CASES) or any(set(entry) != {"case", "status", "nodeAutonomy"} for entry in core["branches"]) or {entry["case"]: entry for entry in core["branches"]} != expected_branches:
        raise ValueError("Canonical restoration must follow all actual finite branch observations")
    rotation = core["restoredAuthorizedRotation"]
    if rotation is not None and (set(rotation) != {"txHash", "serial"} or rotation["txHash"] != lifecycle["operations"][0]["transactionHash"] or rotation["serial"] != lifecycle["operations"][0]["serial"]):
        raise ValueError("Restored key rotation must be the same actual owner-authorized operation")
    sources.extend((file, ROOT / index_path))
    for file in sources:
        publish_file(file, PUBLIC / "branches" / file.name, 256 * 1024)
    return {"status": "PASS", "caseCount": len(BRANCH_CASES), "sessionId": session, "coreRestored": True,
            "indexSha256": digest(ROOT / index_path), "coreRestoredSha256": digest(directory / "core-restored.json"), "absoluteFinality": False}


def inspection_exports(run, report, strict_bundle, lifecycle):
    case = report.get("cases", {}).get("rendered-original-order-inspections", {})
    if case.get("status") != "passed" or any(type(case.get(field)) is not int or case[field] != expected for field, expected in (("count", 4), ("new_proofs", 0), ("raw_broadcast_delta", 0))) or case.get("strict_order_id") != strict_bundle["orderId"][2:] or case.get("manifest") != str(run / "ui-inspections.json"):
        raise ValueError("Original-order inspections require the exact current Node-verified manifest and zero financial side effects")
    manifest = json.loads(regular(run / "ui-inspections.json", 65536))
    fields = {"schemaVersion", "kind", "status", "strictOrderId", "strictIntentId", "generatedProofs", "inspections"}
    if set(manifest) != fields or manifest["schemaVersion"] != 1 or manifest["kind"] != "vdp-rendered-inspections" or manifest["status"] != "PASS" or manifest["strictOrderId"] != strict_bundle["orderId"][2:] or str(uuid.UUID(manifest["strictIntentId"])) != manifest["strictIntentId"] or type(manifest["generatedProofs"]) is not int or manifest["generatedProofs"] != 0 or len(manifest["inspections"]) != 4:
        raise ValueError("Every required rendered inspection must bind the same actual original strict order")
    source = bytes.fromhex(json.loads(regular(run / "settlement/relation-2-window-0.json", 16384))["journal"])
    before = bytes.fromhex(json.loads(regular(run / "settlement/key-binding.json", 16384))["journal"])
    after = bytes.fromhex(json.loads(regular(run / "key-lifecycle/rotate.json", 16384))["journal"])
    keys = [{"keyId": value[18 * 32:19 * 32].hex(), "serial": int.from_bytes(value[19 * 32:20 * 32], "big")} for value in (before, after)]
    if keys[0]["serial"] != 1 or keys[1]["serial"] != 2 or keys[0]["keyId"] == keys[1]["keyId"] or keys[1]["serial"] != lifecycle["operations"][0]["serial"]:
        raise ValueError("Inspection key identities must match the actual original and rotated receiving certificates")
    partial = {"phase": "partial", "paymentState": "withdrawn", "earned": "12", "withdrawn": "12", "refunded": "17", "evidence": "none",
               "windows": [{"state": "earned", "decryptionProven": True}, {"state": "refunded", "decryptionProven": False}], "receiverKey": None}
    unknown = {**partial, "phase": "reconciling", "paymentState": "unknown", "windows": [{"state": "pending", "decryptionProven": False}] * 2}
    expected = ({"phase": None, "paymentState": None, "earned": None, "withdrawn": None, "refunded": None, "evidence": None, "windows": [], "receiverKey": keys[0]},
                partial, unknown, {**partial, "receiverKey": keys[1]})
    index = json.loads(regular(PUBLIC / "branches/index.json", 256 * 1024))
    for name in ("partial-refund", "mixed-held-rotate"):
        branch = next((entry for entry in index["cases"] if entry["case"] == name), None)
        if branch is None or branch["status"] != "EVM_PASS" or any(branch["actual"].get(field) != amount for field, amount in (("earned", "12"), ("withdrawn", "12"), ("refunded", "17"))) or branch["actual"]["phases"] != [3, 4]:
            raise ValueError("Rendered partial and rotated-order states must match their actual original-chain branch ledgers")
    identifiers, screenshots = set(), []
    for position, (value, specification) in enumerate(zip(manifest["inspections"], INSPECTIONS)):
        kind, role, name = specification
        if set(value) != {"id", "kind", "role", "nodePeer", "orderId", "intentId", "screenshot", "sha256", "bytes", "observed", "checks"} or not isinstance(value["id"], str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,160}", value["id"]) or value["id"] in identifiers or value["kind"] != kind or value["role"] != role or value["screenshot"] != name or value["nodePeer"] != source[(10 if role == "web" else 11) * 32:(11 if role == "web" else 12) * 32].hex():
            raise ValueError("Each inspection must name its exact bounded stage, actual Node peer and dedicated screenshot")
        identifiers.add(value["id"])
        if value["orderId"] != (None if position == 0 else manifest["strictOrderId"]) or value["intentId"] != (None if position == 0 else manifest["strictIntentId"]) or value["observed"] != expected[position]:
            raise ValueError("Inspection UI state must match the real partial, unknown or receiving-key state of the original order")
        if any(type(window["decryptionProven"]) is not bool for window in value["observed"]["windows"]) or (value["observed"]["receiverKey"] is not None and type(value["observed"]["receiverKey"]["serial"]) is not int):
            raise ValueError("Inspection decryption flags and receiving-key serial must retain their exact protocol types")
        checks = value["checks"]
        if set(checks) != {"walletLocked", "rawBroadcastDelta", "proofJobsUnchanged", "transactionIntentsUnchanged", "canonicalRestored"} or type(checks["rawBroadcastDelta"]) is not int or checks["rawBroadcastDelta"] != 0 or any(checks[field] is not True for field in ("walletLocked", "proofJobsUnchanged", "transactionIntentsUnchanged", "canonicalRestored")):
            raise ValueError("Inspection cannot unlock a wallet, broadcast, regenerate a proof, replace intent or leave the transport fault active")
        file = UI / "public" / name
        data = regular(file, 10 * 1024 ** 2)
        if type(value["bytes"]) is not int or value["bytes"] != len(data) or not re.fullmatch(r"[0-9a-f]{64}", value["sha256"]) or value["sha256"] != digest(file) or not data.startswith(b"\x89PNG\r\n\x1a\n"):
            raise ValueError("Inspection receipt must bind the actual bounded rendered PNG bytes")
        screenshots.append(file)
    for file in screenshots:
        publish_file(file, PUBLIC / "ui" / file.name, 10 * 1024 ** 2)
    publish_file(run / "ui-inspections.json", PUBLIC / "ui/ui-inspections.json", 65536)
    return {"status": "PASS", "count": 4, "newProofs": 0, "strictOrderId": manifest["strictOrderId"],
            "manifestSha256": digest(PUBLIC / "ui/ui-inspections.json")}


def recovery_evidence(report, strict_bundle, original):
    prefix = report.get("cases", {}).get("paid-provider-sigkill-resumes-committed-prefix-and-discards-uncommitted-tail", {})
    fields = {"status", "order_id", "signal", "exit_code", "wallet_locked", "committed_before", "committed_after", "uncommitted_tail_bytes", "truncate_on_first_valid_append", "original_ciphertext_prefix_matches", "transaction_rows_unchanged", "proof_rows_unchanged", "raw_broadcast_delta", "new_proofs", "original_order"}
    if set(prefix) != fields or prefix["status"] != "passed" or prefix["order_id"] != strict_bundle["orderId"][2:] or prefix["signal"] != "SIGKILL" or any(type(prefix[field]) is not int or prefix[field] != expected for field, expected in (("exit_code", -9), ("committed_before", 8192), ("committed_after", 16384), ("uncommitted_tail_bytes", len(b"uncommitted-delivery-tail" * 7)), ("raw_broadcast_delta", 0), ("new_proofs", 0))) or any(prefix[field] is not True for field in ("wallet_locked", "truncate_on_first_valid_append", "original_ciphertext_prefix_matches", "transaction_rows_unchanged", "proof_rows_unchanged", "original_order")):
        raise ValueError("Actual provider crash recovery must preserve committed encrypted bytes and discard the uncommitted tail without finance or new proofs")
    held = report.get("cases", {}).get("real-paid-held-and-key-published-sigkill-recovery", {})
    if set(held) != {"status", "new_proofs", "original_order", "points"} or held["status"] != "passed" or type(held["new_proofs"]) is not int or held["new_proofs"] != 0 or held["original_order"] is not True or len(held["points"]) != 2:
        raise ValueError("Both existing paid recovery points require actual process SIGKILL evidence")
    for value, point in zip(held["points"], ("held", "key-published")):
        if set(value) != {"point", "order_id", "nodes", "earned_before", "earned_after", "proof_rows_unchanged", "transaction_rows_unchanged", "original_sha256", "original_bytes", "raw_broadcast_delta", "warm_cached_material"} or value["point"] != point or value["order_id"] != strict_bundle["orderId"][2:] or value["earned_before"] != "0" or value["earned_after"] != "0" or value["original_sha256"] != original["sha256"] or type(value["original_bytes"]) is not int or value["original_bytes"] != 130816 or type(value["raw_broadcast_delta"]) is not int or value["raw_broadcast_delta"] != 0 or any(value[field] is not True for field in ("proof_rows_unchanged", "transaction_rows_unchanged", "warm_cached_material")) or len(value["nodes"]) != 2:
            raise ValueError("Paid crash recovery must retain the exact original file, warm material and unchanged financial and proof rows")
        for node, role in zip(value["nodes"], ("R", "P")):
            if set(node) != {"role", "pid", "signal", "exit_code", "wallet_locked"} or node["role"] != role or type(node["pid"]) is not int or node["pid"] <= 0 or node["signal"] != "SIGKILL" or type(node["exit_code"]) is not int or node["exit_code"] != -9 or node["wallet_locked"] is not True:
                raise ValueError("Each paid recovery point must actually kill and restart its exact receiver and provider locked")
        if len({node["pid"] for node in value["nodes"]}) != 2:
            raise ValueError("Receiver and provider recovery evidence must name two distinct actual processes")
    return {"providerPrefix": prefix, "heldAndPublished": held, "scope": "warm cached material on the original order; no cold reproving"}


def principal_evidence(report, strict_bundle):
    client = report.get("cases", {}).get("paid-client-principal-boundaries", {})
    service = report.get("cases", {}).get("paid-service-grant-and-private-alpn-boundaries", {})
    for value, flags in ((client, ("wallets_unlocked", "dag_unchanged_during_checks", "manager_positive_controls")),
                         (service, ("wallets_unlocked", "positive_paid_announcement", "positive_source_and_receiver_order",
                                    "positive_receiver_ciphertext_read", "expired_signature_verified", "financial_objects_settings_dag_work_unchanged"))):
        if value.get("status") != "passed" or value.get("order_id") != strict_bundle["orderId"][2:] or any(value.get(field) is not True for field in flags):
            raise ValueError("Paid authority boundaries require their actual original order and all positive controls")
        if any(type(value.get(field)) is not int or value[field] != 0 for field in ("raw_broadcast_delta", "new_orders", "new_proofs")):
            raise ValueError("Authority checks cannot broadcast, create an order or generate another proof")
        if not isinstance(value.get("before_sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", value["before_sha256"]) or value["before_sha256"] != value.get("after_sha256"):
            raise ValueError("Authority checks must preserve the actual financial and private state")
    if str(uuid.UUID(client["intent_id"])) != client["intent_id"]:
        raise ValueError("Client authority evidence requires its actual original intent")
    nonmember = "this device is not a member of the personal network"
    restricted = "delivery writes and spending require a managing member of your personal network"
    expected = [{"actor": actor, "op": op, "rejection": nonmember if actor == "outsider" else restricted}
                for actor in ("outsider", "member", "local_session") for op in ("prepare", "confirm", "configure")]
    expected += [{"actor": actor, "op": op, "rejection": nonmember}
                 for actor, operations in (("outsider", ("get", "nodes")), ("foreign_manager", ("get", "confirm", "nodes"))) for op in operations]
    expected += [{"actor": actor, "op": "status-get-nodes", "allowed": True} for actor in ("member", "local_session")]
    if client.get("checks") != expected or any(row.get("allowed") is not True for row in client["checks"][-2:]):
        raise ValueError("Every actual client role must exercise its fixed writes, private reads and allowed same-network reads")
    agent = client.get("agent", {})
    expected_agent = [{"entry": entry, "op": op, "rejection": rejection}
                      for entry, operations, rejection in (("agent-tool", ("confirm", "configure", "get"), "unknown or unavailable coordination tool"),
                                                            ("agent-bearer-client", ("confirm", "configure", "get", "nodes"), nonmember)) for op in operations]
    if agent.get("passed") is not True or agent.get("valid_token_before_and_after") is not True or agent.get("checks") != expected_agent:
        raise ValueError("Agent authority needs a valid live token before and after every fixed tool and client rejection")
    completion = client.get("agent_completion", {})
    if completion.get("state") != "completed" or type(completion.get("new_events")) is not int or completion["new_events"] != 1 or type(completion.get("new_work")) is not int or completion["new_work"] != 0 or completion.get("financial_state_unchanged") is not True or not isinstance(completion.get("event_hash"), str) or not re.fullmatch(r"[0-9a-f]{64}", completion["event_hash"]):
        raise ValueError("Only the original Agent's normal completion may follow the side-effect-free authority checks")
    expected_service = {"receiver": ["receiver-valid-read", "receiver-write-grant-wrong-role", "signed-expired-grant"],
                        "source": ["source-read-grant-wrong-role"],
                        "outsider": ["outsider-order", "outsider-grant-role", "outsider-other-peer-grant", "private-rpc-sync-blob-all-three-nodes"]}
    if service.get("checks") != expected_service or service.get("expiry_fixture") != "resigned-provider-host-unix-seconds" or type(service.get("private_alpn_targets")) is not int or service["private_alpn_targets"] != 3 or service.get("private_alpns") != ["rpc", "sync", "blob"]:
        raise ValueError("Service authority needs real paid roles, correctly signed expiry and every private protocol at all three Nodes")
    return {"status": "PASS", "orderId": strict_bundle["orderId"], "clientChecks": 16, "agentChecks": 7,
            "serviceChecks": 8, "privateAlpnChecks": 9, "rawBroadcastDelta": 0, "newOrders": 0, "newProofs": 0,
            "clientStateSha256": client["before_sha256"], "serviceStateSha256": service["before_sha256"],
            "agentCompletionEventHash": completion["event_hash"]}


def public_exports(run, report, compiled):
    original = checked_capacity_case(report)
    ordinary = checked_ordinary_case(report)
    strict_bundle, strict_proofs = settlement_export(run, "settlement", 2, "29")
    ordinary_bundle, ordinary_proofs = settlement_export(run, "ordinary-settlement", 1, "17")
    lifecycle, lifecycle_proofs = lifecycle_export(run, original)
    if lifecycle["oldOrderId"] != strict_bundle["orderId"] or "0x" + ordinary["orderId"] != ordinary_bundle["orderId"] or ordinary["currentRegistrationId"] != lifecycle["finalRegistrationId"] or ordinary_bundle["transactions"]["registerKey"] != lifecycle["operations"][-1]["transactionHash"] or ordinary_proofs[0] != lifecycle_proofs[-1]:
        raise ValueError("Ordinary order must reuse the exact final active key proof and registration transaction")
    unique = strict_proofs + lifecycle_proofs + ordinary_proofs[1:]
    if len(unique) != 13 or len({proof["journalHash"] for proof in unique}) != 13 or len({proof["receiptSha256"] for proof in unique}) != 13 or any(proof["imageId"] != compiled["imageId"] for proof in unique) or len({proof["profileId"] for proof in unique}) != 1:
        raise ValueError("Live acceptance requires thirteen distinct real receipts bound to the actual guest and one profile")
    branches = branch_exports(report, lifecycle, run / "key-lifecycle", original)
    inspections = inspection_exports(run, report, strict_bundle, lifecycle)
    recovery = recovery_evidence(report, strict_bundle, original)
    principals = principal_evidence(report, strict_bundle)
    return {"genuineProofs": 13, "earned": "29", "withdrawn": "29", "paymentScope": "strict maximum-file order",
            "readOnlyObserver": True, "originalFile": original, "ordinary": ordinary,
            "keyLifecycle": {"generatedProofs": 3, "finalActiveSerial": lifecycle["finalActiveSerial"],
                             "finalBindingJournalHash": lifecycle["finalBindingJournalHash"]}, "branches": branches,
            "inspections": inspections, "recovery": recovery, "principals": principals}


def checked_ui_evidence(summary, web, android, report):
    ordinary = summary["ordinary"]
    web_fields = {"status", "orderId", "intentId", "bytes", "sha256", "actualPairing", "actualFileSelection", "actualQuotation", "actualPaymentConfirmation", "reloadPreservedIntent", "settledWhileReceiverAppOffline", "earned", "withdrawn", "refunded", "quotationScreenshot"}
    android_fields = {"status", "orderId", "intentId", "bytes", "sha256", "actualPairing", "actualAcceptance", "backgroundObserved", "receiverReverseRemoved", "existingAppConnectionTerminated", "actualOfflineVisible", "settledWhileBackgrounded", "sameOrderResumed", "actualSavedFile", "offlineScreenshot", "savedScreenshot"}
    for value, fields in ((web, web_fields), (android, android_fields)):
        if set(value) != fields or value["status"] != "passed" or any(value[field] != ordinary[field] for field in ("orderId", "intentId", "bytes", "sha256")) or type(value["bytes"]) is not int:
            raise ValueError("Actual UI evidence must match the original Node order and recovered file")
        if any(value[field] is not True for field in fields - {"status", "orderId", "intentId", "bytes", "sha256", "earned", "withdrawn", "refunded", "quotationScreenshot", "offlineScreenshot", "savedScreenshot"}):
            raise ValueError("Actual UI evidence is missing a required rendered or saved-file observation")
    if any(web[field] != ordinary[field] for field in ("earned", "withdrawn", "refunded")) or web["quotationScreenshot"] != "web.png" or android["offlineScreenshot"] != "android-offline.png" or android["savedScreenshot"] != "android.png":
        raise ValueError("Actual UI evidence has incorrect payment or screenshot identities")
    run = report.get("run", {})
    if report.get("schemaVersion") != "report-1" or run.get("status") != "passed" or run.get("exitCode") != 0 or run.get("errors") != [] or run.get("vcs", {}).get("commit") != summary["sourceCommit"] or run.get("vcs", {}).get("dirty") is not False:
        raise ValueError("The actual e2e runner must finish without run or cleanup errors")
    selected = [result for result in run.get("results", []) if result.get("selected") is True]
    expected = {("vdp-paid-web", "web", "tests/paid-web-transfers.paid.ts"), ("vdp-paid-android", "android", "tests/paid-native-transfers.paid.ts")}
    if len(selected) != 2 or {(result.get("targetId"), result.get("platform"), result.get("file")) for result in selected} != expected or any(result.get("status") != "passed" or len(result.get("attempts", [])) != 1 or result["attempts"][0].get("status") != "passed" for result in selected):
        raise ValueError("Both actual paid UI tests must pass on their exact target without skip or retry")
    if any(run.get("summary", {}).get(field) != expected for field, expected in (("selected", 2), ("executed", 2), ("passed", 2), ("failed", 0), ("interrupted", 0), ("flaky", 0), ("skipped", 0))) or any(run.get("usage", {}).get(field) != 0 for field in ("modelTokens", "maxModelCallsInStep")):
        raise ValueError("Actual paid UI acceptance must execute both tests and use no model calls")
    return {"status": "PASS", "selected": 2, "passed": 2, "modelCalls": 0}


def video_exports(ordinary):
    videos = []
    for role, specifications in VIDEO_SPECS.items():
        receipt = json.loads(regular(UI / "public" / (role + "-video.json"), 16384))
        if set(receipt) != {"schemaVersion", "kind", "status", "role", "orderId", "intentId", "videos"} or receipt["schemaVersion"] != 1 or receipt["kind"] != "vdp-paid-ui-videos" or receipt["status"] != "PASS" or receipt["role"] != role or any(receipt[field] != ordinary[field] for field in ("orderId", "intentId")) or len(receipt["videos"]) != len(specifications):
            raise ValueError("Each paired UI must provide its own exact actual recording receipt for this order")
        for value, (name, stage, source) in zip(receipt["videos"], specifications):
            fields = {"file", "sha256", "bytes", "durationSeconds", "exitCode", "signal", "stop", "source", "stage", "startedAfterPairing"}
            if set(value) != fields or value["file"] != name or value["stage"] != stage or value["source"] != source or value["startedAfterPairing"] is not True or not re.fullmatch(r"[0-9a-f]{64}", value["sha256"]) or type(value["bytes"]) is not int or not 0 < value["bytes"] <= 128 * 1024 ** 2 or type(value["durationSeconds"]) not in (int, float) or not math.isfinite(value["durationSeconds"]) or not 0 < value["durationSeconds"] <= 180:
                raise ValueError("Record only the three bounded post-pairing UI segments with exact public metadata")
            if role == "web":
                stopped = value["stop"] == "q" and type(value["exitCode"]) is int and value["exitCode"] == 0 and value["signal"] is None
            else:
                stopped = value["stop"] == "SIGINT" and ((type(value["exitCode"]) is int and value["exitCode"] in (0, 130) and value["signal"] is None) or (value["exitCode"] is None and value["signal"] == "SIGINT"))
            if not stopped:
                raise ValueError("Recording must retain its real normal quit or explicitly accepted SIGINT outcome")
            file = UI / "public" / name
            if file.is_symlink() or not file.is_file() or file.stat().st_size != value["bytes"] or digest(file) != value["sha256"]:
                raise ValueError("Recording bytes differ from the exact test-produced post-pairing MP4")
            with file.open("rb") as stream:
                header = stream.read(12)
            if header[4:8] != b"ftyp" or header[8:12] not in (b"isom", b"iso2", b"iso5", b"iso6", b"mp41", b"mp42", b"avc1"):
                raise ValueError("The named recording must be an actual MP4 container")
            probed = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=format_name,duration:stream=codec_type,width,height",
                                     "-of", "json", str(file)], capture_output=True, timeout=20)
            if probed.returncode != 0 or len(probed.stdout) > 65536:
                raise ValueError("The actual recording could not be independently inspected")
            media = json.loads(probed.stdout)
            duration = float(media["format"]["duration"])
            if "mp4" not in media["format"]["format_name"].split(",") or not math.isfinite(duration) or not 0 < duration <= 180 or abs(duration - value["durationSeconds"]) > 0.05 or not any(stream.get("codec_type") == "video" and type(stream.get("width")) is int and type(stream.get("height")) is int and 0 < stream["width"] <= 4096 and 0 < stream["height"] <= 4096 for stream in media.get("streams", [])):
                raise ValueError("Recording requires an independently observed bounded MP4 video and matching duration")
            publish_file(file, PUBLIC / "ui" / name, 128 * 1024 ** 2)
            videos.append(value)
    metadata = {"schemaVersion": 1, "kind": "vdp-paid-ui-videos", "status": "PASS", "source": "post-pairing OS recording",
                "orderId": ordinary["orderId"], "intentId": ordinary["intentId"], "videos": videos}
    replace_public(PUBLIC / "ui/video-metadata.json", metadata)
    return {"status": "PASS", "segments": len(videos), "metadataSha256": digest(PUBLIC / "ui/video-metadata.json")}


def finalize_ui(android_provenance):
    summary = json.loads(regular(PUBLIC / "summary.json", 65536))
    resources_report = json.loads(regular(PUBLIC / "resources.json", 16384))
    branches = summary.get("branches", {})
    inspections = summary.get("inspections", {})
    if summary.get("status") != "PROOFS_PASS" or summary.get("genuineProofs") != 13 or branches.get("status") != "PASS" or branches.get("caseCount") != len(BRANCH_CASES) or branches.get("coreRestored") is not True or inspections.get("status") != "PASS" or inspections.get("count") != 4 or inspections.get("newProofs") != 0 or summary.get("sourceCommit") != android_provenance["sourceCommit"] or resources_report.get("resourceStatus") != "PASS" or any(resources_report.get("oomEventsDelta", {}).get(field) != 0 for field in ("oom", "oom_kill", "oom_group_kill")):
        raise ValueError("UI cannot upgrade missing or failed actual proof acceptance")
    manifest_file = PUBLIC / "ui/ui-inspections.json"
    manifest = json.loads(regular(manifest_file, 65536))
    if digest(manifest_file) != inspections["manifestSha256"] or manifest["strictOrderId"] != inspections["strictOrderId"] or len(manifest["inspections"]) != len(INSPECTIONS):
        raise ValueError("Host UI join requires the exact Node-verified original-order inspection manifest")
    for value, (_, _, name) in zip(manifest["inspections"], INSPECTIONS):
        if value["screenshot"] != name:
            raise ValueError("Inspect only the four exact original-order PNG files")
        for file in (UI / "public" / name, PUBLIC / "ui" / name):
            data = regular(file, 10 * 1024 ** 2)
            if len(data) != value["bytes"] or digest(file) != value["sha256"] or not data.startswith(b"\x89PNG\r\n\x1a\n"):
                raise ValueError("Host UI join found changed inspection screenshot bytes")
    web = json.loads(regular(UI / "public/web.json", 16384))
    android = json.loads(regular(UI / "public/android.json", 16384))
    report = json.loads(regular(UI / "e2e/report.json", 8 * 1024 ** 2))
    joined = checked_ui_evidence(summary, web, android, report)
    videos = video_exports(summary["ordinary"])
    for name in UI_FILES:
        source = UI / "public" / name
        if name.endswith(".png") and not regular(source, 10 * 1024 ** 2).startswith(b"\x89PNG\r\n\x1a\n"):
            raise ValueError("Only the three explicit post-join PNG screenshots may be published")
        publish_file(source, PUBLIC / "ui" / name, 10 * 1024 ** 2 if name.endswith(".png") else 16384)
    settled = UI / "public/web-settled.png"
    settled_bytes = regular(settled, 10 * 1024 ** 2)
    if not settled_bytes.startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError("The ordinary Web order requires its actual post-settlement PNG")
    publish_file(settled, PUBLIC / "ui/web-settled.png", 10 * 1024 ** 2)
    summary.update(status="PASS", ui=joined, videos=videos, settledScreenshot={"file": "web-settled.png", "bytes": len(settled_bytes), "sha256": digest(settled)}, android={"sourceCommit": android_provenance["sourceCommit"],
                   "variant": android_provenance["variant"], "applicationId": android_provenance["applicationId"], "apks": android_provenance["apks"]})
    replace_public(PUBLIC / "summary.json", summary)


def mark_host_failure(expected, reason):
    file = PUBLIC / "summary.json"
    try:
        summary = json.loads(regular(file, 65536))
    except (OSError, ValueError):
        summary = {"schemaVersion": 1, "sourceCommit": expected}
    summary.update(status="FAIL", hostFailure=reason)
    replace_public(file, summary)


def checked_capacity_case(report):
    case = report.get("cases", {}).get("two-window-source-hold-open-and-original-file")
    if report.get("status") != "passed" or not isinstance(case, dict):
        raise ValueError("Maximum-file evidence requires the successful actual paid case")
    if any(type(case.get(name)) is not int or case[name] != 130_816 for name in ("bytes", "max_file_bytes")):
        raise ValueError("Paid file evidence must cover the actual advertised 130816-byte boundary")
    if case.get("capacity_boundary") is not True or not isinstance(case.get("sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", case["sha256"]):
        raise ValueError("Paid file capacity evidence or SHA-256 is invalid")
    return {"bytes": case["bytes"], "maxFileBytes": case["max_file_bytes"], "capacityBoundary": True, "sha256": case["sha256"]}


def inside(package, expected):
    if ROOT != Path("/work") or os.uname().machine != "x86_64" or os.environ.get("TOKIO_WORKER_THREADS") != "2":
        raise ValueError("Live acceptance requires the canonical Linux sandbox and two Tokio worker threads")
    subprocess.run(["git", "config", "--global", "--add", "safe.directory", "/work"], check=True)
    source_commit(expected)
    run = ARTIFACTS / ("live-runs/" + uuid.uuid4().hex)
    run.mkdir(parents=True, mode=0o700)
    owner = [os.environ.get("VIBEFACE_DELIVERY_UI_" + field, "") for field in ("UID", "GID")]
    if any(not re.fullmatch(r"[0-9]{1,10}", value) or int(value) >= 2 ** 32 for value in owner):
        raise ValueError("The host must supply bounded numeric UI owner IDs")
    public_ui = PUBLIC / "ui"
    if PUBLIC.is_symlink() or not PUBLIC.is_dir() or PUBLIC.stat().st_uid != int(owner[0]) or PUBLIC.stat().st_gid != int(owner[1]) or set(PUBLIC.iterdir()) != {public_ui} or public_ui.is_symlink() or not public_ui.is_dir() or public_ui.stat().st_uid != int(owner[0]) or public_ui.stat().st_gid != int(owner[1]) or list(public_ui.iterdir()):
        raise ValueError("The host must create one fresh public directory owned by its task user")
    log = run / "private-task.log"
    cgroup, initial = resources()
    summary = {"schemaVersion": 1, "status": "FAIL", "sourceCommit": expected, "mode": "local TestToken on loopback chain 31337",
               "notChecked": ["real assets or public deployment", "absolute chain finality", "iOS paid delivery"]}
    environment = {**os.environ, "CARGO_BUILD_JOBS": "2", "TOKIO_WORKER_THREADS": "2"}
    started = time.monotonic()
    try:
        for component in ("crypto", "web"):
            command("verify " + component + " build inputs", ["python3", "scripts/build-inputs.py", "check", component], log)
        info = checked_package(package, expected)
        binary, module = package / "vibeface", package / "web/vibeface_secure.js"
        provenance = "same-run verified Linux package"
        if not runtime(binary, module, run / "packaged-runtime", log, environment):
            flags = subprocess.check_output(["python3", "scripts/release-rustflags.py"], cwd=ROOT, env={**environment, "CARGO_TARGET_DIR": "/artifacts/node-target"}, text=True).strip()
            command("build current Node for sandbox libc", ["cargo", "+1.98.1", "build", "--release", "--locked", "-p", "vibeface", "--target-dir", "/artifacts/node-target"],
                    log, env={**environment, "CARGO_ENCODED_RUSTFLAGS": flags})
            binary = Path("/artifacts/node-target/release/vibeface")
            if not runtime(binary, module, run / "rebuilt-runtime", log, environment):
                raise RuntimeError("Source-built Node failed the sandbox runtime check")
            provenance = "actual current-source build after explicit packaged GLIBC incompatibility"
        summary["node"] = {"origin": provenance, "sha256": digest(binary), "packageSourceCommit": info["source_commit"],
                           "webRuntimePassed": True, "wasmSha256": digest(package / "web/vibeface_secure_bg.wasm")}
        command("install fixed guest compiler", ["python3", "scripts/install-delivery-toolchain.py"], log, env=environment)
        command("build canonical actual guest and host", ["bash", "proofs/delivery/build.sh"], log, env=environment)
        prover = Path("/artifacts/proof-target/release/vibeface-delivery-prover")
        compiled = json.loads(subprocess.check_output([str(prover), "info"], cwd=ROOT, env=environment))
        checks = runpy.run_path(str(ROOT / "scripts/check-delivery.py"))
        profile = checks["load_profile"](compiled)
        summary["guest"] = compiled
        probe_target = Path("/artifacts/delivery-probe-target")
        command("build real source-bound QUIC probe", ["cargo", "+1.98.1", "build", "--manifest-path", "tests/fixtures/delivery-probe/Cargo.toml", "--locked", "--target-dir", str(probe_target)],
                log, env={**environment, "VIBEFACE_PROBE_SOURCE_ROOT": "/work"})
        command("install local EVM dependencies", ["node", "contracts/delivery/install.mjs"], log)
        configuration = run / "acceptance.json"
        write_private(configuration, {"schema_version": 1, "deployment_file": str(run / "deployment.json"),
            "prover": {"program": str(prover), "r0vm": "/opt/vdp/bin/r0vm", "cgroup": str(cgroup), "profile": profile},
            "max_service_amount": "1000000", "source_gas_cap_wei": "1000000000000000000",
            "provider_gas_cap_wei": "1000000000000000000", "register_key_gas_cap_wei": "1000000000000000000",
            "settlement_observer": "/work/contracts/delivery/observe-settlement.mjs"})
        try:
            command("thirteen genuine proofs, actual Web and Android order and independent observers", ["python3", "scripts/verify-node.py", "--delivery", "--binary", str(binary),
                "--crypto-module", str(module), "--artifacts", str(run / "process"), "--delivery-probe", str(probe_target / "debug/vibeface-delivery-probe"),
                "--delivery-config", str(configuration), "--delivery-source-root", "/work", "--delivery-profile", "/work/proofs/delivery/profile.json",
                "--delivery-ui-control", str(UI / "control.json"), "--delivery-ui-timeout-ms", str(UI_TIMEOUT_MS)], log, env=environment)
        except RuntimeError:
            summary["failureStage"] = "paid-process-acceptance"
            try:
                preserved = preserve_strict_evidence(run, compiled, profile, prover)
                if preserved is not None:
                    summary["partialEvidence"] = preserved
            except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
                print("UNVERIFIED delivery-live strict-only partial evidence unavailable", flush=True)
            raise
        results = list((run / "process").glob("delivery-paid-*/summary.json"))
        if len(results) != 1:
            raise ValueError("Expected exactly one actual paid process acceptance result")
        evidence = public_exports(results[0].parent, json.loads(regular(results[0])), compiled)
        summary.update(status="PROOFS_PASS", **evidence)
    finally:
        final, delta, resource_failure = None, None, None
        try:
            _, final = resources()
            delta = oom_delta(initial, final)
            if any(delta.values()):
                resource_failure = "oom_events_increased"
        except (OSError, ValueError, KeyError):
            resource_failure = "resource_observation_failed"
        if resource_failure:
            summary.update(status="FAIL", resourceFailure=resource_failure)
        write_private(PUBLIC / "summary.json", summary)
        write_private(PUBLIC / "resources.json", {"scope": "entire sandbox including builds, Nodes, EVM and local prover", "initial": initial,
                      "final": final, "oomEventsDelta": delta, "resourceStatus": "FAIL" if resource_failure else "PASS",
                      "elapsedSeconds": time.monotonic() - started, "sourceCommit": expected})
        for name in ("summary.json", "resources.json"):
            (PUBLIC / name).chmod(0o644)
        if resource_failure:
            raise RuntimeError("Live delivery resource acceptance failed: " + resource_failure)
    print("PASS delivery-live thirteen genuine proofs; awaiting the host UI process join", flush=True)


def self_test_ui(directory):
    from copy import deepcopy
    from unittest.mock import Mock, patch

    def rejected(operation):
        try:
            operation()
        except (ValueError, OSError, KeyError, RuntimeError, TimeoutError):
            return
        raise AssertionError("Invalid UI, APK or process evidence must fail")

    release = directory / "release"
    release.mkdir()
    apk = release / "app-release.apk"
    with zipfile.ZipFile(apk, "w") as archive:
        for abi, machine in ANDROID_ABIS.items():
            header = bytearray(20)
            header[:6] = b"\x7fELF\x02\x01"
            header[18:20] = machine.to_bytes(2, "little")
            archive.writestr(f"lib/{abi}/libvibeface-secure-native.so", header + b"self-test ELF fixture; not an Android build")
    (release / "output-metadata.json").write_text(json.dumps({"variantName": "release", "applicationId": "one.leaper.vibeface", "elements": [{"outputFile": apk.name}]}))
    with patch(f"{__name__}.ROOT", directory), patch(f"{__name__}.source_commit"), patch(f"{__name__}.android_tool", return_value="/fixture/apkanalyzer"), patch("subprocess.check_output", side_effect=["one.leaper.vibeface\n", "false\n"]):
        record_android(release, "1" * 40)
        staged = directory / ".artifacts/mobile-android"
        actual, provenance = checked_android(staged, "1" * 40)
        assert actual == (staged / apk.name).resolve()
        assert len(provenance["apks"][0]["nativeLibraries"]) == 2
        rejected(lambda: checked_android(staged, "2" * 40))
        (staged / (apk.name + ".sha256")).write_text("0" * 64 + "  " + apk.name + "\n")
        rejected(lambda: checked_android(staged, "1" * 40))
        (staged / (apk.name + ".sha256")).write_text(provenance["apks"][0]["sha256"] + "  " + apk.name + "\n")
        with (staged / apk.name).open("ab") as output:
            output.write(b"foreign-build")
        rejected(lambda: checked_android(staged, "1" * 40))
    foreign = directory / "foreign.apk"
    with zipfile.ZipFile(foreign, "w") as archive:
        for abi in ANDROID_ABIS:
            archive.writestr(f"lib/{abi}/libvibeface-secure-native.so", b"\x7fELF\x02\x01" + bytes(14))
    rejected(lambda: apk_native_libraries(foreign))
    ui = directory / "ui"
    ui.mkdir(mode=0o700)
    descriptor = ui / "control.json"
    value = {"schemaVersion": 1, "url": "http://127.0.0.1:12345", "token": "a" * 64}
    write_private(descriptor, value)
    with patch(f"{__name__}.UI", ui):
        assert control_descriptor(descriptor) == value
        with patch("http.client.HTTPConnection") as connection:
            connection.return_value.getresponse.return_value.status = 200
            connection.return_value.getresponse.return_value.read.return_value = b'{"ok":true,"result":{"actual":true}}'
            assert controller(value, "/facts") == {"actual": True}
            connection.assert_called_once_with("127.0.0.1", 12345, timeout=10)
            connection.return_value.getresponse.return_value.status = 302
            rejected(lambda: controller(value, "/facts"))
            connection.return_value.close.assert_called()
        descriptor.chmod(0o644)
        rejected(lambda: control_descriptor(descriptor))
        descriptor.chmod(0o600)
        with patch("os.getuid", return_value=os.getuid() + 1):
            rejected(lambda: control_descriptor(descriptor))
        for url in ("http://localhost:12345", "http://example.com:12345", "http://user:pass@127.0.0.1:12345", "http://127.0.0.1:12345/other", "https://127.0.0.1:12345"):
            descriptor.write_text(json.dumps({**value, "url": url}))
            rejected(lambda: control_descriptor(descriptor))
        descriptor.unlink()
        descriptor.symlink_to(apk)
        rejected(lambda: control_descriptor(descriptor))
        descriptor.unlink()
        ready = Mock(poll=Mock(return_value=0))
        running = Mock(poll=Mock(return_value=None))
        failed = Mock(poll=Mock(return_value=1))
        with patch("time.monotonic", return_value=100):
            assert remaining_ui_timeout_ms(100.75) == 750
            rejected(lambda: remaining_ui_timeout_ms(100))
            rejected(lambda: remaining_ui_timeout_ms(100 + LIVE_TIMEOUT_SECONDS + 1))
        with patch(f"{__name__}.controller", side_effect=OSError("closed private controller")) as abort, patch("time.monotonic", side_effect=[100, 100.5, 101]), patch("time.sleep") as pause:
            finish_failed_ui_evidence(running, failed, value, 101)
            abort.assert_called_once_with(value, "/abort", {"role": "web"})
            assert pause.call_count == 1
        with patch(f"{__name__}.controller") as abort, patch("time.monotonic", side_effect=[100, 160]), patch("time.sleep") as pause:
            original = RuntimeError("original paid UI failure")
            try:
                try:
                    raise original
                except RuntimeError:
                    finish_failed_ui_evidence(running, failed, value, 500)
                    raise
            except RuntimeError as raised:
                assert raised is original
            pause.assert_not_called()
        with patch(f"{__name__}.controller") as abort:
            finish_failed_ui_evidence(running, ready, value, time.monotonic() + 10)
            abort.assert_not_called()
        join_owned(ready, ready, running, time.monotonic() + 10)
        for processes in ((failed, running, running), (running, failed, running), (ready, ready, ready)):
            rejected(lambda: join_owned(*processes, time.monotonic() + 10))
        rejected(lambda: join_owned(running, running, running, time.monotonic() - 1))
        rejected(lambda: wait_owned("failed process", time.monotonic() + 10, lambda: True, (failed,)))
        host = Mock()
        with patch(f"{__name__}.terminate_owned") as stop, patch(f"{__name__}.controller") as abort, patch("subprocess.run", return_value=Mock(returncode=0)) as docker:
            assert cleanup_owned(host, running, running, "vdp-live-self-test", value, False) is False
            abort.assert_called_once_with(value, "/abort", {"role": "web"})
            assert stop.call_args_list[0].args == (running,) and stop.call_count == 2
            assert docker.call_args.args[0] == ["docker", "stop", "--time", "10", "vdp-live-self-test"]
            host.close.assert_called_once()
        host = Mock(close=Mock(side_effect=RuntimeError("owned AVD cleanup failure")))
        with patch(f"{__name__}.terminate_owned", side_effect=[OSError("UI cleanup failure"), None]) as stop, patch("subprocess.run", side_effect=OSError("Docker cleanup failure")):
            assert cleanup_owned(host, running, running, "vdp-live-self-test", None, True) is True
            assert stop.call_count == 2
            host.close.assert_called_once()
    with (
        patch(f"{__name__}.UI", directory / "android-start"),
        patch(f"{__name__}.android_tool", side_effect=lambda name: "/bootstrap-sdk/cmdline-tools/20.0/bin/" + name) as bootstrap,
        patch(f"{__name__}.command") as setup,
        patch(f"{__name__}.available_port", return_value=12345),
        patch(f"{__name__}.port_free", return_value=True),
        patch(f"{__name__}.wait_owned"),
        patch("subprocess.Popen"),
    ):
        host = AndroidHost()
        host.adb = Mock(side_effect=["", "", "", "0", "x86_64"])
        try:
            environment = host.start()
            install, create = setup.call_args_list
            tool = Path(create.args[1][0])
            assert tool.name == "avdmanager" and tool.parents[3] == host.sdk
            assert "cmdline-tools;" + tool.parents[1].name in install.args[1]
            assert "--sdk_root=" + str(host.sdk) in install.args[1]
            assert environment["ANDROID_SDK_ROOT"] == environment["ANDROID_HOME"] == str(host.sdk)
            bootstrap.assert_called_once_with("sdkmanager")
        finally:
            if host.log is not None:
                host.log.close()
    host = AndroidHost()
    host.server_port, host.serial = 12345, "emulator-5554"
    host.reverse = {"tcp:43210": "tcp:43210", "tcp:43211": None}
    host.adb = Mock(side_effect=["fixture tcp:43210 tcp:55555\nfixture tcp:43211 tcp:43211", "", "", "fixture tcp:43211 tcp:43211", "", "", ""])
    host.close()
    assert host.adb.call_args_list[1].args == ("-s", "emulator-5554", "reverse", "--remove", "tcp:43210")
    assert host.adb.call_args_list[2].args == ("-s", "emulator-5554", "reverse", "tcp:43210", "tcp:43210")
    assert not any("--remove-all" in call.args for call in host.adb.call_args_list)
    host = AndroidHost()
    host.server_port, host.serial, host.reverse = 12345, "emulator-5554", {"tcp:43210": None}
    host.adb = Mock(return_value="")
    host.close()
    assert not any("--remove" in call.args for call in host.adb.call_args_list)
    with patch("os.killpg") as kill:
        process = Mock(pid=987654, poll=Mock(return_value=None), wait=Mock(side_effect=[subprocess.TimeoutExpired("fixture", 10), 0]))
        terminate_owned(process)
        assert [call.args for call in kill.call_args_list] == [(987654, signal.SIGTERM), (987654, signal.SIGKILL)]
    ordinary = {"order_id": "d" * 64, "intent_id": str(uuid.uuid4()), "bytes": 4096, "sha256": "e" * 64,
                "windows": 1, "acceptance": "recoverable", "new_proofs": 3, "public_proofs": 4,
                "current_registration": "key-register/" + str(uuid.uuid4()), "earned": "17", "withdrawn": "17", "refunded": "0"}
    ordinary.update({field: True for field in ("actual_web_confirmation", "actual_android_acceptance", "actual_android_saved_file", "offline_visible", "settled_while_app_background", "read_only_observer")})
    ordinary["ui_events"] = {event: {"order_id": ordinary["order_id"], "intent_id": ordinary["intent_id"], "monotonic": index + 1.0} for index, event in enumerate(UI_EVENTS)}
    paid = {"status": "passed", "cases": {"ordinary-paid-real-web-android-offline-recovery-and-save": ordinary}}
    summary = {"sourceCommit": "1" * 40, "ordinary": checked_ordinary_case(paid)}
    common = {"status": "passed", "orderId": ordinary["order_id"], "intentId": ordinary["intent_id"], "bytes": 4096, "sha256": ordinary["sha256"], "actualPairing": True}
    web = {**common, "actualFileSelection": True, "actualQuotation": True, "actualPaymentConfirmation": True, "reloadPreservedIntent": True, "settledWhileReceiverAppOffline": True, "earned": "17", "withdrawn": "17", "refunded": "0", "quotationScreenshot": "web.png"}
    android = {**common, **{field: True for field in ("actualAcceptance", "backgroundObserved", "receiverReverseRemoved", "existingAppConnectionTerminated", "actualOfflineVisible", "settledWhileBackgrounded", "sameOrderResumed", "actualSavedFile")}, "offlineScreenshot": "android-offline.png", "savedScreenshot": "android.png"}
    results = [{"targetId": target, "platform": platform, "file": file, "selected": True, "status": "passed", "attempts": [{"status": "passed"}]} for target, platform, file in (("vdp-paid-web", "web", "tests/paid-web-transfers.paid.ts"), ("vdp-paid-android", "android", "tests/paid-native-transfers.paid.ts"))]
    report = {"schemaVersion": "report-1", "run": {"status": "passed", "exitCode": 0, "errors": [], "vcs": {"commit": "1" * 40, "dirty": False}, "results": results,
              "summary": {"selected": 2, "executed": 2, "passed": 2, "failed": 0, "interrupted": 0, "flaky": 0, "skipped": 0}, "usage": {"modelTokens": 0, "maxModelCallsInStep": 0}}}
    assert checked_ui_evidence(summary, web, android, report)["passed"] == 2
    for changed in ({**web, "token": "private-token"}, {**web, "orderId": "f" * 64}, {**web, "actualPaymentConfirmation": False}):
        rejected(lambda: checked_ui_evidence(summary, changed, android, report))
    rejected(lambda: checked_ui_evidence(summary, web, {**android, "actualSavedFile": False}, report))
    for field, value in (("status", "failed"), ("results", []), ("usage", {"modelTokens": 0, "maxModelCallsInStep": 1}), ("vcs", {"commit": "2" * 40, "dirty": False})):
        rejected(lambda: checked_ui_evidence(summary, web, android, {**report, "run": {**report["run"], field: value}}))
    changed = deepcopy(report)
    changed["run"]["results"][0]["attempts"].append({"status": "passed"})
    rejected(lambda: checked_ui_evidence(summary, web, android, changed))
    for field, value in (("new_proofs", 4), ("actual_web_confirmation", False), ("earned", "29"), ("bytes", 4096.0)):
        changed = deepcopy(paid)
        changed["cases"]["ordinary-paid-real-web-android-offline-recovery-and-save"][field] = value
        rejected(lambda: checked_ordinary_case(changed))
    changed = deepcopy(paid)
    changed["cases"]["ordinary-paid-real-web-android-offline-recovery-and-save"]["ui_events"]["saved"]["order_id"] = "f" * 64
    rejected(lambda: checked_ordinary_case(changed))
    return paid


def self_test_exports(directory, report):
    from copy import deepcopy
    from unittest.mock import patch
    run, public = directory / "proof-export-fixtures", directory / "public-export-fixtures"
    public.mkdir()
    for name in ("settlement", "ordinary-settlement", "key-lifecycle"):
        (run / name).mkdir(parents=True)
    original = {"bytes": 130816, "max_file_bytes": 130816, "capacity_boundary": True, "sha256": "a" * 64}
    report["cases"]["two-window-source-hold-open-and-original-file"] = original
    nonmember = "this device is not a member of the personal network"
    restricted = "delivery writes and spending require a managing member of your personal network"
    client_checks = [{"actor": actor, "op": op, "rejection": error}
                     for actor, error in (("outsider", nonmember), ("member", restricted), ("local_session", restricted))
                     for op in ("prepare", "confirm", "configure")]
    client_checks += [{"actor": "outsider", "op": op, "rejection": nonmember} for op in ("get", "nodes")]
    client_checks += [{"actor": "foreign_manager", "op": op, "rejection": nonmember} for op in ("get", "confirm", "nodes")]
    client_checks += [{"actor": actor, "op": "status-get-nodes", "allowed": True} for actor in ("member", "local_session")]
    agent_checks = [{"entry": "agent-tool", "op": op, "rejection": "unknown or unavailable coordination tool"} for op in ("confirm", "configure", "get")]
    agent_checks += [{"entry": "agent-bearer-client", "op": op, "rejection": nonmember} for op in ("confirm", "configure", "get", "nodes")]
    client_case = {"status": "passed", "order_id": "a" * 64, "intent_id": str(uuid.uuid4()), "checks": client_checks,
                   "agent": {"passed": True, "valid_token_before_and_after": True, "checks": agent_checks},
                   "before_sha256": "c" * 64, "after_sha256": "c" * 64, "wallets_unlocked": True,
                   "raw_broadcast_delta": 0, "new_orders": 0, "new_proofs": 0, "dag_unchanged_during_checks": True,
                   "manager_positive_controls": True,
                   "agent_completion": {"state": "completed", "new_events": 1, "event_hash": "e" * 64, "new_work": 0, "financial_state_unchanged": True}}
    service_case = {"status": "passed", "order_id": "a" * 64,
                    "checks": {"receiver": ["receiver-valid-read", "receiver-write-grant-wrong-role", "signed-expired-grant"],
                               "source": ["source-read-grant-wrong-role"],
                               "outsider": ["outsider-order", "outsider-grant-role", "outsider-other-peer-grant", "private-rpc-sync-blob-all-three-nodes"]},
                    "positive_paid_announcement": True, "positive_source_and_receiver_order": True,
                    "positive_receiver_ciphertext_read": True, "expired_signature_verified": True,
                    "expiry_fixture": "resigned-provider-host-unix-seconds", "private_alpn_targets": 3, "private_alpns": ["rpc", "sync", "blob"],
                    "before_sha256": "d" * 64, "after_sha256": "d" * 64, "wallets_unlocked": True,
                    "raw_broadcast_delta": 0, "new_orders": 0, "new_proofs": 0, "financial_objects_settings_dag_work_unchanged": True}
    client_name, service_name = "paid-client-principal-boundaries", "paid-service-grant-and-private-alpn-boundaries"
    report["cases"].update({client_name: client_case, service_name: service_case})
    bundle = {"orderId": "0x" + "a" * 64}
    assert principal_evidence(report, bundle)["privateAlpnChecks"] == 9
    for case_name, field, invalid in ((client_name, "raw_broadcast_delta", False), (client_name, "checks", client_checks[:-1]),
                                      (client_name, "agent", {**client_case["agent"], "valid_token_before_and_after": False}),
                                      (service_name, "after_sha256", "f" * 64), (service_name, "expired_signature_verified", 1),
                                      (service_name, "private_alpns", ["rpc", "sync"]), (service_name, "status", "skipped")):
        changed = deepcopy(report)
        changed["cases"][case_name][field] = invalid
        try:
            principal_evidence(changed, bundle)
        except ValueError:
            pass
        else:
            raise AssertionError("Missing, untrusted or state-changing authority checks cannot become public PASS evidence")
    private_marker = "private-principal-fixture-must-not-export"
    report["cases"][client_name]["private_diagnostic"] = private_marker
    assert private_marker not in json.dumps(principal_evidence(report, bundle))
    ordinary_case = report["cases"]["ordinary-paid-real-web-android-offline-recovery-and-save"]
    adapter = directory / "contracts/delivery/RiscZeroAdapter.sol"
    adapter.parent.mkdir(parents=True)
    adapter.write_text("bytes4 public constant SELECTOR = 0x73c457ba;\n")

    def fixture_facts(parent, name, relation):
        with patch(f"{__name__}.ROOT", directory):
            return public_proof(parent, name, relation)

    def fixture_proof(parent, name, relation, identifier, serial=1, action=1):
        journal = bytearray(1504)
        for index, value in ((1, relation), (19, serial), (45, identifier), (46, action)):
            journal[32 * index:32 * (index + 1)] = value.to_bytes(32, "big")
        journal[128:160], journal[160:192], journal[704:736] = bytes.fromhex("c" * 64), bytes.fromhex("b" * 64), bytes.fromhex("e" * 64)
        journal[10 * 32:11 * 32], journal[11 * 32:12 * 32], journal[18 * 32:19 * 32] = bytes.fromhex("1" * 64), bytes.fromhex("2" * 64), bytes([serial + 2]) * 32
        value = {"mode": "prove", "kind": "groth16", "relation": relation, "imageId": "b" * 64, "profileId": "c" * 64,
                 "journal": journal.hex(), "journalHash": hashlib.sha256(journal).hexdigest(), "seal": "73c457ba" + "00" * 256}
        write_private(parent / (name + ".json"), value)
        (parent / (name + ".receipt")).write_bytes(b"self-test export fixture, not a cryptographic proof: " + str(identifier).encode())
        return fixture_facts(parent, name, relation)

    operations = []
    for index, action in enumerate(("rotate", "revoke", "register")):
        proof = fixture_proof(run / "key-lifecycle", action, 1, 8 + index, serial=2 + index, action=2 if action == "revoke" else 1)
        intent = str(uuid.uuid4())
        operation = {"action": action, "bindingAction": "revoked" if action == "revoke" else "active", "serial": 2 + index,
                     "intentId": intent, "bindingHash": "e" * 64, "transactionId": "key-register/" + intent,
                     "transactionHash": "0x" + format(300 + index, "064x"), "relation": 1, "public": action + ".json", "receipt": action + ".receipt",
                     "journalHash": proof["journalHash"], "receiptSha256": proof["receiptSha256"], "newProofs": 1,
                     "registryConfirmed": True, "retryReusedIntent": True, "restartedWalletLocked": True, "oldOrderReadSha256": "a" * 64}
        operations.append(operation)
    ordinary_case["current_registration"] = operations[-1]["transactionId"]
    lifecycle = {"schemaVersion": 1, "kind": "genuine-key-lifecycle", "status": "PASS", "generatedProofs": 3, "operations": operations,
                 "finalRegistrationId": operations[-1]["transactionId"], "finalActiveSerial": operations[-1]["serial"], "finalBindingJournalHash": operations[-1]["journalHash"],
                 "imageId": "b" * 64, "profileId": "c" * 64, "oldOrderId": "0x" + "a" * 64, "oldFileSha256": "a" * 64, "mixedHeldObservation": "mixed-held.json"}
    write_private(run / "key-lifecycle/summary.json", lifecycle)
    mixed = {"schemaVersion": 1, "sessionId": "self-test-only", "case": "mixed-held-rotate", "status": "EVM_PASS", "helperNewProofs": 0, "newOwnerKeyBindings": 1,
             "state": {"anchor": {"number": 1, "hash": "0x" + "f" * 64, "timestamp": 1}, "tokenBalances": {"0x" + "1" * 40: "100"}, "tokenMode": 0, "tokenDecimals": 6,
             "order": {"earned": "12", "withdrawn": "12", "refunded": "17", "bonusEarned": False, "bonusRefunded": True,
             "windows": [{"phase": phase, "decryptionProven": proven, "capsule": "0x", "capsuleHash": "0x" + "0" * 64} for phase, proven in ((3, True), (4, False))]}},
             "txHash": operations[0]["transactionHash"], "nodeObservations": [{"checkpointId": "self-test-rotation-mixed-held-result", "roles": ["R"], "status": "observed", "rawBroadcastDelta": 0, "nativeRefund": False, "keyRotation": False}],
             "transactions": [{"label": "fixture-rotate", "hash": operations[0]["transactionHash"], "status": 1, "blockNumber": 1, "blockHash": "0x" + "f" * 64, "gasUsed": "1", "executionGasFeeWei": "1"}],
             "oldFileSha256": "a" * 64, "imageId": "0x" + "b" * 64, "profileId": "0x" + "c" * 64, "restoreRequired": True, "nodeAutonomy": "REAL_OWNER_ROTATE_ONLY_KEEPER_OLD_WINDOW_SETTLEMENT"}
    write_private(run / "key-lifecycle/mixed-held.json", mixed)
    for directory_name, windows, amount, base, order in (("settlement", 2, "29", 100, "a" * 64), ("ordinary-settlement", 1, "17", 200, ordinary_case["order_id"])):
        destination = run / directory_name
        entries = [{"relation": 1, "public": "key-binding.json", "receipt": "key-binding.receipt"}]
        if directory_name == "settlement":
            fixture_proof(destination, "key-binding", 1, 1)
        else:
            for extension in (".json", ".receipt"):
                shutil.copyfile(run / "key-lifecycle" / ("register" + extension), destination / ("key-binding" + extension))
        for index in range(windows):
            for relation in (2, 3, 4):
                name = f"relation-{relation}-window-{index}"
                fixture_proof(destination, name, relation, 2 + 3 * index + relation - 2 if windows == 2 else 11 + relation - 2)
                entries.append({"relation": relation, "windowIndex": index, "public": name + ".json", "receipt": name + ".receipt"})
        transactions = {"registerKey": "0x" + format(base, "064x"), "approve": "0x" + format(base + 1, "064x"), "fund": "0x" + format(base + 2, "064x")}
        for offset, action in enumerate(("hold", "publish", "open", "withdraw")):
            transactions[action] = ["0x" + format(base + 3 + offset * 2 + index, "064x") for index in range(windows)]
        if directory_name == "ordinary-settlement":
            transactions["registerKey"] = operations[-1]["transactionHash"]
        bundle = {"schemaVersion": 1, "escrow": "0x" + "1" * 40, "asset": "0x" + "2" * 40, "orderId": "0x" + order, "proofs": entries,
                  "capsules": [{"windowIndex": index, "capsule": "00" * 80} for index in range(windows)], "transactions": transactions}
        write_private(destination / "settlement.json", bundle)
        facts = [fixture_facts(destination, Path(entry["public"]).stem, entry["relation"]) for entry in entries]
        observation = {"schemaVersion": 1, "status": "PASS", "orderId": bundle["orderId"], "money": {"funded": amount, "earned": amount, "withdrawn": amount, "refunded": "0"},
                       "proofs": [{"journalHash": "0x" + fact["journalHash"], "receiptSha256": "0x" + fact["receiptSha256"]} for fact in facts]}
        write_private(destination / "settlement-observation.json", observation)
    session = "self-test-session"
    branch_directory = directory / ".artifacts/vdp/contracts/branch-observations" / session
    branch_directory.mkdir(parents=True)
    policy = {"kind": "experimental-block-depth", "confirmations": 2, "observedTip": 2, "anchorNumber": 1, "anchorHash": "0x" + "f" * 64, "absoluteFinality": False}
    core_state = deepcopy(mixed["state"])
    core_state["order"].update(earned="29", withdrawn="29", refunded="0", bonusEarned=True, bonusRefunded=False)
    for window in core_state["order"]["windows"]:
        window.update(phase=3, decryptionProven=True)
    case_entries, key_entries = [], []
    for name in BRANCH_CASES:
        incoming = name.startswith("incoming-")
        if name == "mixed-held-rotate":
            record = {**mixed, "sessionId": session}
            (run / "key-lifecycle/mixed-held.json").write_text(json.dumps(record))
        else:
            record = {"schemaVersion": 1, "sessionId": session, "case": name, "status": "EVM_PASS", "newProofs": 0, "originalGenuineProofSet": 7,
                      "state": deepcopy(core_state), "imageId": "0x" + "b" * 64, "profileId": "0x" + "c" * 64,
                      "confirmationPolicy": policy, "transactions": [], "nodeObservations": [], "nodeAutonomy": "SELF_TEST_EXPORT_FORMAT_ONLY"}
            if name == "partial-refund":
                record["state"] = deepcopy(mixed["state"])
            if incoming:
                successful = name in ("incoming-reentrancy", "incoming-nonreturn")
                record.update(snapshotKind="C0Prime", actualFundReceiptStatus=int(successful), nodeAutonomy="NOT_TESTED_ORIGINAL_BROADCAST_RAW_ONLY")
                record["state"]["order"] = deepcopy(core_state["order"]) if successful else None
                if successful:
                    record["state"]["order"].update(earned="0", withdrawn="0", refunded="0", bonusEarned=False)
                    for window in record["state"]["order"]["windows"]:
                        window.update(phase=0, decryptionProven=False)
        file = branch_directory / ("branch-" + name + ".json")
        write_private(file, record)
        if name == "mixed-held-rotate":
            (run / "key-lifecycle/mixed-held.json").write_bytes(file.read_bytes())
        actual = checked_branch_state(record["state"])
        if incoming:
            actual["fundReceiptStatus"] = record["actualFundReceiptStatus"]
        node = {"status": "OBSERVED", "roles": ["R"]} if name == "mixed-held-rotate" else {"status": "NOT_OBSERVED", "roles": []}
        case_entries.append({"case": name, "status": "EVM_PASS", "file": file.name, "fileSha256": digest(file), "expected": actual,
                             "actual": actual, "nodeObservation": node, "nodeAutonomy": record["nodeAutonomy"]})
    for operation in operations:
        action = operation["action"]
        metadata = json.loads((run / "key-lifecycle" / (action + ".json")).read_bytes())
        record = {"schemaVersion": 1, "sessionId": session, "case": action, "status": "PASS", "serial": operation["serial"],
                  "bindingAction": 2 if action == "revoke" else 1, "bindingHash": "0x" + operation["bindingHash"], "txHash": operation["transactionHash"],
                  "registryConfirmed": True, "helperNewProofs": 0, "proofGenerationOwner": "acceptance", "imageId": "0x" + "b" * 64,
                  "profileId": "0x" + "c" * 64, "proof": {"relation": 1, "journal": metadata["journal"], "journalHash": metadata["journalHash"], "seal": metadata["seal"], "verification": "production-adapter"},
                  "blockNumber": 1, "blockHash": "0x" + "f" * 64, "gasUsed": "1", "executionGasFeeWei": "1", "absoluteFinality": False,
                  "oldOrderReadSha256": "a" * 64, "mixedHeld": "PASS" if action == "rotate" else "NOT_TESTED"}
        file = branch_directory / ("key-" + action + ".json")
        write_private(file, record)
        key_entries.append({"case": action, "status": "PASS", "file": file.name, "txHash": operation["transactionHash"], "fileSha256": digest(file)})
    core = {"schemaVersion": 1, "sessionId": session, "event": "core-restored", "id": "self-test-core", "status": "PASS", "scope": "original strict core only",
            "originalHashesAndNonces": True, "restoredAuthorizedRotation": {"txHash": operations[0]["transactionHash"], "serial": operations[0]["serial"]},
            "branches": [{field: entry[field] for field in ("case", "status", "nodeAutonomy")} for entry in case_entries], "imageId": "0x" + "b" * 64,
            "profileId": "0x" + "c" * 64, "confirmationPolicy": policy, "state": core_state, "oldFileSha256": "a" * 64, "absoluteFinality": False}
    write_private(branch_directory / "core-restored.json", core)
    index = {"schemaVersion": 1, "sessionId": session, "kind": "genuine-settlement-branch-index", "imageId": "0x" + "b" * 64, "profileId": "0x" + "c" * 64,
             "requiredCases": list(BRANCH_CASES), "coreRestored": True, "restore": {"status": "PASS", "file": "core-restored.json", "fileSha256": digest(branch_directory / "core-restored.json"), "confirmationAnchor": policy},
             "cases": case_entries, "lifecycle": key_entries, "absoluteFinality": False}
    write_private(branch_directory / "index.json", index)
    report["cases"]["genuine-branches-and-canonical-restoration"] = {"status": "passed", "session_id": session, "helper_new_proofs": 0, "core_restored": True,
        "public_index": str(Path(".artifacts/vdp/contracts/branch-observations") / session / "index.json")}
    ui = directory / "inspection-ui"
    (ui / "public").mkdir(parents=True)
    strict_intent = str(uuid.uuid4())
    partial_observed = {"phase": "partial", "paymentState": "withdrawn", "earned": "12", "withdrawn": "12", "refunded": "17", "evidence": "none",
                        "windows": [{"state": "earned", "decryptionProven": True}, {"state": "refunded", "decryptionProven": False}], "receiverKey": None}
    states = ({"phase": None, "paymentState": None, "earned": None, "withdrawn": None, "refunded": None, "evidence": None, "windows": [], "receiverKey": {"keyId": "03" * 32, "serial": 1}},
              partial_observed, {**partial_observed, "phase": "reconciling", "paymentState": "unknown", "windows": [{"state": "pending", "decryptionProven": False}] * 2},
              {**partial_observed, "receiverKey": {"keyId": "04" * 32, "serial": 2}})
    inspections = []
    for position, (kind, role, name) in enumerate(INSPECTIONS):
        file = ui / "public" / name
        file.write_bytes(b"\x89PNG\r\n\x1a\nself-test byte fixture; not a rendered UI")
        inspections.append({"id": "self-test-" + kind, "kind": kind, "role": role, "nodePeer": ("1" if role == "web" else "2") * 64,
            "orderId": None if position == 0 else "a" * 64, "intentId": None if position == 0 else strict_intent,
            "screenshot": name, "sha256": digest(file), "bytes": file.stat().st_size, "observed": states[position],
            "checks": {"walletLocked": True, "rawBroadcastDelta": 0, "proofJobsUnchanged": True, "transactionIntentsUnchanged": True, "canonicalRestored": True}})
    manifest = {"schemaVersion": 1, "kind": "vdp-rendered-inspections", "status": "PASS", "strictOrderId": "a" * 64, "strictIntentId": strict_intent, "generatedProofs": 0, "inspections": inspections}
    write_private(run / "ui-inspections.json", manifest)
    report["cases"]["rendered-original-order-inspections"] = {"status": "passed", "manifest": str(run / "ui-inspections.json"), "count": 4, "new_proofs": 0, "raw_broadcast_delta": 0, "strict_order_id": "a" * 64}
    report["cases"]["paid-provider-sigkill-resumes-committed-prefix-and-discards-uncommitted-tail"] = {"status": "passed", "order_id": "a" * 64, "signal": "SIGKILL", "exit_code": -9, "wallet_locked": True,
        "committed_before": 8192, "committed_after": 16384, "uncommitted_tail_bytes": 175, "truncate_on_first_valid_append": True, "original_ciphertext_prefix_matches": True,
        "transaction_rows_unchanged": True, "proof_rows_unchanged": True, "raw_broadcast_delta": 0, "new_proofs": 0, "original_order": True}
    report["cases"]["real-paid-held-and-key-published-sigkill-recovery"] = {"status": "passed", "new_proofs": 0, "original_order": True,
        "points": [{"point": point, "order_id": "a" * 64, "nodes": [{"role": role, "pid": pid, "signal": "SIGKILL", "exit_code": -9, "wallet_locked": True} for role, pid in (("R", 100), ("P", 101))],
                    "earned_before": "0", "earned_after": "0", "proof_rows_unchanged": True, "transaction_rows_unchanged": True, "original_sha256": "a" * 64, "original_bytes": 130816,
                    "raw_broadcast_delta": 0, "warm_cached_material": True} for point in ("held", "key-published")]}
    with patch(f"{__name__}.PUBLIC", public), patch(f"{__name__}.ROOT", directory), patch(f"{__name__}.UI", ui):
        assert public_exports(run, report, {"imageId": "b" * 64})["genuineProofs"] == 13
        assert json.loads((public / "ui/ui-inspections.json").read_bytes())["generatedProofs"] == 0
        for position, field, value in ((2, "observed", partial_observed), (3, "nodePeer", "f" * 64), (0, "screenshot", "../private-node.png")):
            changed = deepcopy(manifest)
            changed["inspections"][position][field] = value
            (run / "ui-inspections.json").write_text(json.dumps(changed))
            try:
                public_exports(run, report, {"imageId": "b" * 64})
            except ValueError:
                pass
            else:
                raise AssertionError("Inspection must show the actual unknown state, correct Node and exact public PNG")
        (run / "ui-inspections.json").write_text(json.dumps(manifest))
        for field, value in (("rawBroadcastDelta", False), ("walletLocked", 1), ("canonicalRestored", False)):
            changed = deepcopy(manifest)
            changed["inspections"][2]["checks"][field] = value
            (run / "ui-inspections.json").write_text(json.dumps(changed))
            try:
                public_exports(run, report, {"imageId": "b" * 64})
            except ValueError:
                pass
            else:
                raise AssertionError("Inspection requires exact boolean safeguards, integer zero broadcasts and restored transport")
        (run / "ui-inspections.json").write_text(json.dumps(manifest))
        changed = deepcopy(report)
        changed["cases"]["real-paid-held-and-key-published-sigkill-recovery"]["points"][0]["nodes"][1]["role"] = "R"
        try:
            public_exports(run, changed, {"imageId": "b" * 64})
        except ValueError:
            pass
        else:
            raise AssertionError("Recovery must include both actual receiver and provider, not duplicate roles")
        mixed_source = run / "key-lifecycle/mixed-held.json"
        original_mixed = mixed_source.read_bytes()
        unprefixed = json.loads(original_mixed)
        unprefixed["nodeObservations"][0]["checkpointId"] = "mixed-held-result"
        mixed_source.write_text(json.dumps(unprefixed))
        try:
            mixed_held_export(mixed_source.parent, checked_capacity_case(report), lifecycle)
        except ValueError:
            pass
        else:
            raise AssertionError("An unprefixed legacy checkpoint must not replace the actual command-owned mixed-held result")
        mixed_source.write_bytes(original_mixed)
        assert len(list((public / "branches").iterdir())) == len(BRANCH_CASES) + 5
        for field, value in (("coreRestored", False), ("cases", case_entries[:-1]), ("requiredCases", list(BRANCH_CASES)[:-1])):
            (branch_directory / "index.json").write_text(json.dumps({**index, field: value}))
            try:
                public_exports(run, report, {"imageId": "b" * 64})
            except ValueError:
                pass
            else:
                raise AssertionError("Missing cases or incomplete canonical restoration must block full paid acceptance")
        (branch_directory / "index.json").write_text(json.dumps(index))
        changed = deepcopy(core)
        changed["state"]["order"]["withdrawn"] = "28"
        (branch_directory / "core-restored.json").write_text(json.dumps(changed))
        changed_index = deepcopy(index)
        changed_index["restore"]["fileSha256"] = digest(branch_directory / "core-restored.json")
        (branch_directory / "index.json").write_text(json.dumps(changed_index))
        try:
            public_exports(run, report, {"imageId": "b" * 64})
        except ValueError:
            pass
        else:
            raise AssertionError("A matching file checksum cannot replace actual complete core ledger restoration")
        (branch_directory / "core-restored.json").write_text(json.dumps(core, indent=2) + "\n")
        (branch_directory / "index.json").write_text(json.dumps(index))
        proof_file = run / "key-lifecycle/rotate.json"
        original_proof = json.loads(proof_file.read_bytes())
        for seal in (original_proof["seal"][:-2], original_proof["seal"] + "00", "00000000" + original_proof["seal"][8:]):
            proof_file.write_text(json.dumps({**original_proof, "seal": seal}))
            try:
                public_proof(proof_file.parent, "rotate", 1)
            except ValueError:
                pass
            else:
                raise AssertionError("Short, long or foreign-selector public seals must fail")
        proof_file.write_text(json.dumps(original_proof))
        safe_mixed = json.loads((public / "key-lifecycle/mixed-held.json").read_text())
        assert safe_mixed["nodeObservations"][0]["keyRotation"] is False and safe_mixed["restoreRequired"] is True
        assert not any("raw" in file.name or "witness" in file.name for file in public.rglob("*"))
        changed_serial = deepcopy(lifecycle)
        changed_serial["operations"][0]["serial"] = 20
        for changed in ({**lifecycle, "generatedProofs": 10}, {**lifecycle, "finalRegistrationId": "another-register"}, changed_serial):
            (run / "key-lifecycle/summary.json").write_text(json.dumps(changed))
            try:
                public_exports(run, report, {"imageId": "b" * 64})
            except ValueError:
                pass
            else:
                raise AssertionError("Incorrect lifecycle count or active registration must fail")
        (run / "key-lifecycle/summary.json").write_text(json.dumps(lifecycle))
        original_observation = (run / "ordinary-settlement/settlement-observation.json").read_bytes()
        (run / "ordinary-settlement/settlement-observation.json").write_bytes((run / "settlement/settlement-observation.json").read_bytes())
        try:
            public_exports(run, report, {"imageId": "b" * 64})
        except ValueError:
            pass
        else:
            raise AssertionError("A strict observer cannot stand in for the ordinary order's independent receipt")
        (run / "ordinary-settlement/settlement-observation.json").write_bytes(original_observation)
        (run / "ordinary-settlement/relation-2-window-0.receipt").write_bytes((run / "settlement/relation-2-window-0.receipt").read_bytes())
        matching_observer = json.loads(original_observation)
        matching_observer["proofs"][1]["receiptSha256"] = "0x" + digest(run / "ordinary-settlement/relation-2-window-0.receipt")
        (run / "ordinary-settlement/settlement-observation.json").write_text(json.dumps(matching_observer))
        try:
            public_exports(run, report, {"imageId": "b" * 64})
        except ValueError:
            pass
        else:
            raise AssertionError("A reused receipt cannot count as a newly generated ordinary proof")


def self_test_partial_evidence(directory):
    from unittest.mock import Mock, patch
    run = directory / "failed-paid-run"
    source = run / "process/delivery-paid-fixture/settlement"
    source.mkdir(parents=True)
    for name in [*(name + extension for name in PROOFS for extension in (".json", ".receipt")), "settlement.json", "settlement-observation.json"]:
        shutil.copyfile(directory / "proof-export-fixtures/settlement" / name, source / name)
    write_private(source.parent / "summary.json", {"status": "failed", "privateFixture": "must never be exported"})
    profile = {"guest_image_id": [187] * 32, "statement_schema_hash": [221] * 32, "crypto_suite": 1, "max_windows": 2,
               "max_window_bytes": 65536, "max_total_bytes": 131072, "max_guest_cycles": 64000000, "max_prover_memory_bytes": 16 * 1024 ** 3}
    encoded = hashlib.sha256(b"vibeface-delivery-profile-v1").digest() + bytes(profile["guest_image_id"]) + bytes(profile["statement_schema_hash"])
    for field in ("crypto_suite", "max_windows", "max_window_bytes", "max_total_bytes", "max_guest_cycles", "max_prover_memory_bytes"):
        encoded += profile[field].to_bytes(32, "big")
    profile_id = hashlib.sha256(encoded).hexdigest()
    observed = json.loads((source / "settlement-observation.json").read_bytes())
    verified = {}
    for position, name in enumerate(PROOFS):
        file = source / (name + ".json")
        value = json.loads(file.read_bytes())
        journal = bytearray.fromhex(value["journal"])
        journal[128:160] = bytes.fromhex(profile_id)
        value.update(profileId=profile_id, journal=journal.hex(), journalHash=hashlib.sha256(journal).hexdigest())
        file.write_text(json.dumps(value))
        observed["proofs"][position]["journalHash"] = "0x" + value["journalHash"]
        verified[value["journal"]] = {"verified": True, "kind": "groth16", "imageId": "b" * 64, "journalHash": value["journalHash"], "seal": value["seal"]}
    (source / "settlement-observation.json").write_text(json.dumps(observed))
    public = directory / "strict-only-public"
    public.mkdir()

    def actual_host_command(arguments, **options):
        assert arguments[:2] == ["/fixture-current-host", "verify"] and isinstance(options["input"], bytes)
        return Mock(returncode=0, stdout=json.dumps(verified[arguments[2]]).encode())

    with patch(f"{__name__}.ROOT", directory), patch(f"{__name__}.PUBLIC", public), patch("subprocess.run", side_effect=actual_host_command) as host:
        partial = preserve_strict_evidence(run, {"imageId": "b" * 64}, profile, Path("/fixture-current-host"))
        assert partial["scope"] == "strict-only" and partial["genuineProofs"] == 7 and partial["canonicalRestorationVerified"] is False
        assert host.call_count == 7 and len(list(public.iterdir())) == 16 and not (public / "summary.json").exists()
        for file in public.iterdir():
            file.unlink()
        for wrong in ({**profile, "max_windows": 3}, profile):
            host.side_effect = actual_host_command if wrong is not profile else None
            host.return_value = Mock(returncode=1, stdout=b"")
            try:
                preserve_strict_evidence(run, {"imageId": "b" * 64}, wrong, Path("/fixture-current-host"))
            except ValueError:
                pass
            else:
                raise AssertionError("A foreign profile or failed actual host verify must not export partial receipts")
            assert not list(public.iterdir())
        host.side_effect = actual_host_command
        observation = source / "settlement-observation.json"
        contents = observation.read_bytes()
        observation.unlink()
        try:
            preserve_strict_evidence(run, {"imageId": "b" * 64}, profile, Path("/fixture-current-host"))
        except (OSError, ValueError):
            pass
        else:
            raise AssertionError("Strict-only evidence requires its actual successful independent observer")
        assert not list(public.iterdir())
        observation.write_bytes(contents)
        real_replace, calls = os.replace, []

        def failed_move(source_file, destination):
            calls.append(destination)
            if len(calls) == 2:
                raise OSError("synthetic partial publication failure")
            return real_replace(source_file, destination)

        with patch("os.replace", side_effect=failed_move):
            try:
                preserve_strict_evidence(run, {"imageId": "b" * 64}, profile, Path("/fixture-current-host"))
            except OSError:
                pass
            else:
                raise AssertionError("Failed partial publication must retain its original error")
        assert not list(public.iterdir())


def self_test_videos(directory):
    from copy import deepcopy
    from unittest.mock import Mock, patch
    ui, public = directory / "video-fixture-ui", directory / "video-fixture-public"
    (ui / "public").mkdir(parents=True)
    public.mkdir()
    ordinary = {"orderId": "a" * 64, "intentId": str(uuid.uuid4())}
    receipts = {}
    for role, specifications in VIDEO_SPECS.items():
        videos = []
        for name, stage, source in specifications:
            file = ui / "public" / name
            file.write_bytes(b"\x00\x00\x00\x18ftypisom" + b"self-test metadata fixture; not a real video")
            videos.append({"file": name, "sha256": digest(file), "bytes": file.stat().st_size, "durationSeconds": 1.5,
                           "exitCode": 130 if stage == "android-resume-save" else 0, "signal": None, "stop": "q" if role == "web" else "SIGINT",
                           "source": source, "stage": stage, "startedAfterPairing": True})
        receipt = {"schemaVersion": 1, "kind": "vdp-paid-ui-videos", "status": "PASS", "role": role, **ordinary, "videos": videos}
        receipts[role] = receipt
        write_private(ui / "public" / (role + "-video.json"), receipt)
    (ui / "public/private-trace.json").write_text("private self-test fixture must never be copied")
    media = {"format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "1.500"}, "streams": [{"codec_type": "video", "width": 1440, "height": 1000}]}
    with patch(f"{__name__}.UI", ui), patch(f"{__name__}.PUBLIC", public), patch("subprocess.run", return_value=Mock(returncode=0, stdout=json.dumps(media).encode())) as probe:
        assert video_exports(ordinary)["segments"] == 3
        assert all(call.args[0][0] == "ffprobe" for call in probe.call_args_list)
        assert {file.name for file in (public / "ui").iterdir()} == {"video-metadata.json", *(specification[0] for specifications in VIDEO_SPECS.values() for specification in specifications)}
        bad = []
        for role, field, value in (("web", "startedAfterPairing", False), ("web", "exitCode", 130), ("web", "sha256", "0" * 64), ("web", "durationSeconds", 181), ("android", "signal", "SIGTERM")):
            changed = deepcopy(receipts[role])
            changed["videos"][0][field] = value
            bad.append((role, changed))
        bad.append(("web", {**receipts["web"], "token": "private-self-test-marker"}))
        for role, changed in bad:
            file = ui / "public" / (role + "-video.json")
            file.write_text(json.dumps(changed))
            try:
                video_exports(ordinary)
            except ValueError:
                pass
            else:
                raise AssertionError("Missing pairing, foreign recording metadata, bad hash or failed stop must block video acceptance")
            file.write_text(json.dumps(receipts[role]))
        for probed in (Mock(returncode=1, stdout=b""), Mock(returncode=0, stdout=json.dumps({**media, "format": {**media["format"], "duration": "2.0"}}).encode())):
            probe.return_value = probed
            try:
                video_exports(ordinary)
            except ValueError:
                pass
            else:
                raise AssertionError("The host must independently inspect the actual MP4 and matching duration")


def self_test():
    from copy import deepcopy
    from unittest.mock import patch
    with patch("subprocess.check_output", side_effect=["1" * 40, ""]):
        source_commit("1" * 40)
    for actual, dirty in (("2" * 40, ""), ("1" * 40, " M tracked.cpp\n"), ("1" * 40, "?? generated.json\n")):
        with patch("subprocess.check_output", side_effect=[actual, dirty]):
            try:
                source_commit("1" * 40)
            except ValueError as error:
                details = json.loads(str(error).split(": ", 1)[1])
                assert details == {"expected": "1" * 40, "actual": actual, "changes": dirty.splitlines()}
            else:
                raise AssertionError("Wrong commits and both tracked and untracked drift must fail with bounded filename diagnostics")
    checks = runpy.run_path(str(ROOT / "scripts/check-delivery.py"))
    _, original = checks["sandbox_commands"](Path("/context"))
    package = ROOT / ".artifacts/vdp/live-input/package"
    command_line = live_container(original, package, "1" * 40, "vdp-live-self-test")
    assert command_line.count("--mount") == 4 and "--privileged" not in command_line
    assert command_line.count("--network") == 1 and command_line[command_line.index("--network") + 1] == "host"
    assert "--user" not in command_line and not any(argument.startswith("HOME=") for argument in command_line)
    assert command_line[command_line.index("--memory") + 1] == command_line[command_line.index("--memory-swap") + 1] == "11g"
    assert command_line[command_line.index("--pids-limit") + 1] == "64"
    assert command_line[command_line.index("--env") + 1] == "TOKIO_WORKER_THREADS=2"
    assert command_line[-1] == "1" * 40 and command_line[-3] == str(Path("/work/.artifacts/vdp/live-input/package"))
    assert command_line.count(original[-4]) == 1
    assert not any("sock" in argument or "RISC0_DEV_MODE" in argument or "RISC0_SKIP_BUILD" in argument for argument in command_line)
    snapshot = {"memory.events": "low 0\nhigh 0\nmax 1\noom 2\noom_kill 1\noom_group_kill 0\n"}
    assert not any(oom_delta(snapshot, {"memory.events": snapshot["memory.events"].replace("max 1", "max 99")}).values())
    for name, before in (("oom", 2), ("oom_kill", 1), ("oom_group_kill", 0)):
        changed = {"memory.events": snapshot["memory.events"].replace(f"{name} {before}\n", f"{name} {before + 1}\n")}
        assert oom_delta(snapshot, changed)[name] == 1
    for events in ("oom 1\noom_kill 1\n", "oom 2\n", "oom 2\noom_kill -1\n"):
        try:
            oom_delta(snapshot, {"memory.events": events})
        except ValueError:
            pass
        else:
            raise AssertionError("Decreasing, missing or invalid OOM counters must fail")
    case_name = "two-window-source-hold-open-and-original-file"
    report = {"status": "passed", "cases": {case_name: {"bytes": 130_816, "max_file_bytes": 130_816, "capacity_boundary": True,
              "sha256": "a" * 64, "name": "private-name", "content_base64": "private-content", "witness": "private-witness",
              "private_path": "/private/node", "raw_transaction": "private-transaction"}}}
    assert checked_capacity_case(report) == {"bytes": 130_816, "maxFileBytes": 130_816, "capacityBoundary": True, "sha256": "a" * 64}
    invalid_reports = [{**report, "status": "failed"}, {"status": "passed", "cases": {}}]
    for field, value in (("bytes", 98_304), ("max_file_bytes", 98_304), ("bytes", 130816.0), ("capacity_boundary", 1), ("capacity_boundary", False), ("sha256", "A" * 64), ("sha256", "a" * 63)):
        changed = deepcopy(report)
        changed["cases"][case_name][field] = value
        invalid_reports.append(changed)
    for changed in invalid_reports:
        try:
            checked_capacity_case(changed)
        except ValueError:
            pass
        else:
            raise AssertionError("Failed, old-size or invalid capacity evidence must not be exported as success")
    with tempfile.TemporaryDirectory(prefix="live-self-test-", dir=ARTIFACTS) as directory:
        paid = self_test_ui(Path(directory))
        self_test_exports(Path(directory), paid)
        self_test_partial_evidence(Path(directory))
        self_test_videos(Path(directory))
        file = Path(directory) / "private.json"
        write_private(file, {"configured": True})
        assert file.stat().st_mode & 0o777 == 0o600 and json.loads(regular(file)) == {"configured": True}
        log = Path(directory) / "process.log"
        log.write_text("business or fixture failure")
        assert not glibc_failure(Path(directory))
        log.write_text("vibeface: symbol lookup error: lib.so: undefined symbol: function, version GLIBC_2.39")
        assert glibc_failure(Path(directory))
        with patch(f"{__name__}.command", side_effect=RuntimeError("business failure")), patch(f"{__name__}.glibc_failure", return_value=False):
            try:
                runtime(Path("/binary"), Path("/module"), Path(directory), log, {})
            except RuntimeError:
                pass
            else:
                raise AssertionError("Business failures must not enable a source-build fallback")
        with patch(f"{__name__}.command", side_effect=RuntimeError("loader failure")), patch(f"{__name__}.glibc_failure", return_value=True):
            assert runtime(Path("/binary"), Path("/module"), Path(directory), log, {}) is False
        with patch(f"{__name__}.ROOT", Path(directory)):
            generated = Path(directory) / "crates/node/web/generated"
            generated.mkdir(parents=True)
            sample = Path(directory) / "package"
            (sample / "web").mkdir(parents=True)
            (sample / "vibeface").write_bytes(b"source-provenance-fixture-only")
            for name in ("vibeface_secure.js", "vibeface_secure_bg.wasm"):
                (generated / name).write_bytes(b"same-run-crypto")
                (sample / "web" / name).write_bytes(b"same-run-crypto")
            info = {"version": "test", "target": "x86_64-unknown-linux-gnu", "source_commit": "1" * 40, "update_protocol": 1, "data_epoch": 3}
            (sample / "BUILD-INFO.json").write_text(json.dumps(info))
            checked_package(sample, "1" * 40)
            for malformed in ({**info, "source_commit": "2" * 40}, {**info, "target": "another-target"}):
                (sample / "BUILD-INFO.json").write_text(json.dumps(malformed))
                try:
                    checked_package(sample, "1" * 40)
                except ValueError:
                    pass
                else:
                    raise AssertionError("Stale or foreign package provenance must fail")
            (sample / "BUILD-INFO.json").write_text(json.dumps(info))
            (sample / "web/vibeface_secure.js").write_bytes(b"different-build")
            try:
                checked_package(sample, "1" * 40)
            except ValueError:
                pass
            else:
                raise AssertionError("A different crypto artifact must fail")
    assert len(PROOFS) == len(set(PROOFS)) == 7
    print("Live dispatch, APK/ABI, UI/video evidence, process join/cleanup, capacity, OOM and GLIBC self-tests passed; no Android, UI, recording, container or proof ran")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path)
    parser.add_argument("--android", type=Path)
    parser.add_argument("--record-android-apk", type=Path)
    parser.add_argument("--source-commit")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        ARTIFACTS.mkdir(parents=True, exist_ok=True)
        self_test()
    elif args.record_android_apk and args.source_commit and not (args.package or args.android or args.inside):
        record_android(args.record_android_apk.resolve(strict=True), args.source_commit)
    elif args.package and args.source_commit and (args.inside or args.android) and not args.record_android_apk:
        inside(args.package.resolve(strict=True), args.source_commit) if args.inside else dispatch(args.package.resolve(strict=True), args.android.resolve(strict=True), args.source_commit)
    else:
        parser.error("Provide --package, --android and --source-commit, or --record-android-apk and --source-commit")
