#!/usr/bin/env python3
"""Run protected native acceptance and validate its public evidence."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import signal
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid

# Local helper imports must not dirty the checkout before provenance checks.
sys.dont_write_bytecode = True
from release import verify_build_info
from unittest import TestCase


ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / ".artifacts/vdp"


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
    verify_build_info(info)
    if any(info[field] != expected for field in ("source_commit", "core_source_commit", "app_source_commit")) or info["target"] != "x86_64-unknown-linux-gnu":
        raise ValueError("Package provenance differs from this Linux source commit")
    for binary in (package / "vibeface", package / "vibeface-core"):
        if not binary.is_file() or binary.is_symlink():
            raise ValueError("Package CLI/Core binary is missing")
    for name in ("_shell.html", "index.html", "_redirects", "_headers", ".vibeface-build.json"):
        if not (package / "web" / name).is_file():
            raise ValueError("Package static output is incomplete")
    for name in ("vibeface_secure.js", "vibeface_secure_bg.wasm"):
        generated = ROOT / "apps/app/generated" / name
        if digest(package / "web" / name) != digest(generated):
            raise ValueError("Packaged cryptography differs from the same-run verified Web/WASM artifact")
    return info


def unpack_package(directory, destination):
    archives = list(directory.glob("*.tar.gz"))
    if len(archives) != 1:
        raise ValueError("Exactly one same-run Linux package archive is required")
    archive = archives[0]
    checksum = regular(archive.with_name(archive.name + ".sha256")).decode().strip().split()
    if len(checksum) != 2 or checksum[0] != digest(archive) or checksum[1].lstrip("*") != archive.name:
        raise ValueError("Linux package archive checksum mismatch")
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


def replace_public(file, value):
    temporary = file.with_name("." + file.name + "." + uuid.uuid4().hex)
    write_private(temporary, value)
    temporary.chmod(0o644)
    os.replace(temporary, file)


def self_test_source_cli(directory):
    repo = directory / "source-cli"
    scripts = repo / "scripts"
    scripts.mkdir(parents=True)
    for name in ("run-delivery-live.py", "release.py"):
        shutil.copy2(ROOT / "scripts" / name, scripts / name)

    def git(*args):
        return subprocess.run(["git", "-c", "core.hooksPath=/dev/null", *args],
                              cwd=repo, check=True, capture_output=True, text=True)

    git("init", "-q")
    git("add", "scripts")
    git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")
    expected = git("rev-parse", "HEAD").stdout.strip()

    environment = os.environ.copy()
    environment.pop("PYTHONDONTWRITEBYTECODE", None)
    environment.pop("PYTHONPYCACHEPREFIX", None)

    def verify(commit, succeeds):
        result = subprocess.run([sys.executable, str(scripts / "run-delivery-live.py"),
                                 "--verify-source-commit", commit], cwd=repo, env=environment,
                                capture_output=True, text=True)
        assert (result.returncode == 0) == succeeds, result.stderr

    verify(expected, True)
    assert not git("status", "--porcelain").stdout
    verify("0" * 40, False)
    extra = repo / "untracked.txt"
    extra.write_text("untracked source")
    verify(expected, False)
    extra.unlink()
    with (scripts / "release.py").open("a") as stream:
        stream.write("\n# changed source\n")
    verify(expected, False)


def self_test():
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
    snapshot = {"memory.events": "low 0\nhigh 0\nmax 1\noom 2\noom_kill 1\noom_group_kill 0\n"}
    assert not any(oom_delta(snapshot, {"memory.events": snapshot["memory.events"].replace("max 1", "max 99")}).values())
    for name, before in (("oom", 2), ("oom_kill", 1), ("oom_group_kill", 0)):
        changed = {"memory.events": snapshot["memory.events"].replace(f"{name} {before}\n", f"{name} {before + 1}\n")}
        assert oom_delta(snapshot, changed)[name] == 1
    for events in ("oom 1\noom_kill 1\n", "oom 2\n", "oom 2\noom_kill -1\n"):
        with TestCase().assertRaises(ValueError, msg='Decreasing, missing or invalid OOM counters must fail'):
            oom_delta(snapshot, {"memory.events": events})
    with tempfile.TemporaryDirectory(prefix="live-self-test-", dir=ARTIFACTS) as directory:
        self_test_source_cli(Path(directory))
        file = Path(directory) / "private.json"
        write_private(file, {"configured": True})
        assert file.stat().st_mode & 0o777 == 0o600 and json.loads(regular(file)) == {"configured": True}
        log = Path(directory) / "process.log"
        log.write_text("business or fixture failure")
        assert not glibc_failure(Path(directory))
        log.write_text("vibeface: symbol lookup error: lib.so: undefined symbol: function, version GLIBC_2.39")
        assert glibc_failure(Path(directory))
        with patch(f"{__name__}.command", side_effect=RuntimeError("business failure")), patch(f"{__name__}.glibc_failure", return_value=False):
            with TestCase().assertRaises(RuntimeError, msg='Business failures must not enable a source-build fallback'):
                runtime(Path("/binary"), Path("/module"), Path(directory), log, {})
        with patch(f"{__name__}.command", side_effect=RuntimeError("loader failure")), patch(f"{__name__}.glibc_failure", return_value=True):
            assert runtime(Path("/binary"), Path("/module"), Path(directory), log, {}) is False
        with patch(f"{__name__}.ROOT", Path(directory)):
            generated = Path(directory) / "apps/app/generated"
            generated.mkdir(parents=True)
            sample = Path(directory) / "package"
            (sample / "web").mkdir(parents=True)
            (sample / "vibeface").write_bytes(b"source-provenance-fixture-only")
            (sample / "vibeface-core").write_bytes(b"source-provenance-core-fixture-only")
            for name in ("_shell.html", "index.html", "_redirects", "_headers", ".vibeface-build.json"):
                (sample / "web" / name).write_text("fixture")
            for name in ("vibeface_secure.js", "vibeface_secure_bg.wasm"):
                (generated / name).write_bytes(b"same-run-crypto")
                (sample / "web" / name).write_bytes(b"same-run-crypto")
            info = {"version": "0.0.1", "target": "x86_64-unknown-linux-gnu", "source_commit": "1" * 40,
                    "update_protocol": 1, "data_epoch": 1, "core_version": "0.0.1", "core_source_commit": "1" * 40,
                    "app_version": "0.0.1", "app_source_commit": "1" * 40}
            (sample / "BUILD-INFO.json").write_text(json.dumps(info))
            checked_package(sample, "1" * 40)
            (sample / "BUILD-INFO.json").write_text(json.dumps({**info, "core_version": "2.3.4", "app_version": "3.4.5-rc.2"}))
            checked_package(sample, "1" * 40)
            for malformed in ({**info, "source_commit": "2" * 40}, {**info, "target": "another-target"},
                              {**info, "core_source_commit": "2" * 40}, {**info, "app_source_commit": "3" * 40}):
                (sample / "BUILD-INFO.json").write_text(json.dumps(malformed))
                with TestCase().assertRaises(ValueError, msg='Stale or foreign package provenance must fail'):
                    checked_package(sample, "1" * 40)
            (sample / "BUILD-INFO.json").write_text(json.dumps(info))
            (sample / "web/vibeface_secure.js").write_bytes(b"different-build")
            with TestCase().assertRaises(ValueError, msg='A different crypto artifact must fail'):
                checked_package(sample, "1" * 40)
    self_test_protected(original)
    protected_selfcheck()
    print("Protected package, dispatch and export self-tests passed; no Node, container or proof ran")


def protected_container(original, package, expected, run, common, git_directory, name):
    relative_git = git_directory.relative_to(common)
    for value in (ROOT, ROOT / ".git", common):
        if "," in str(value):
            raise ValueError("Docker bind paths cannot contain commas")
    result = original[:-4]
    result += ["--name", name, "--user", f"{os.getuid()}:{os.getgid()}", "--mount", f"type=bind,source={ROOT / '.git'},target=/work/.git,readonly",
               "--mount", f"type=bind,source={common},target=/git-common,readonly"]
    for key, value in {
        "TOKIO_WORKER_THREADS": "2", "GIT_OPTIONAL_LOCKS": "0",
        "GIT_DIR": str(Path("/git-common") / relative_git), "GIT_WORK_TREE": "/work",
        "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "safe.directory", "GIT_CONFIG_VALUE_0": "/work",
    }.items():
        result += ["--env", key + "=" + value]
    return result + [original[-4], "python3", "scripts/run-delivery-live.py", "--protected-inside",
        "--protected-package", str(Path("/work") / package.relative_to(ROOT)), "--source-commit", expected,
        "--protected-run-dir", str(Path("/work") / run.relative_to(ROOT))]


def protected_public_failure(run, expected, returncode):
    output = Path(tempfile.mkdtemp(prefix="public-export-", dir=run)); output.chmod(0o755)
    replace_public(output / "summary.json", {"schemaVersion":1,"status":"FAIL","sourceCommit":expected,
        "scope":"fresh-two-order-protected-native-only","genuineReceipts":0,"nativeStatus":"UNVERIFIED",
        "restartObserverStatus":"UNVERIFIED","nativeExitCode":None,"observerExitCode":None,
        "containerExitCode":returncode,"failureCodes":["public_export_unavailable"]})
    return output, False


OBSERVER_SCRIPT = "scripts/verify-delivery-restart-observer.py"
OBSERVER_READY_FD = "VIBEFACE_PROTECTED_OBSERVER_READY_FD"
NATIVE_DONE_FD = "VIBEFACE_PROTECTED_NATIVE_DONE_FD"
OBSERVER_CHECKS = ("counterReadyToStopped", "otherMethodsZero", "counterNondecreasing", "writeRpcZero",
                   "proverIdentityUnchanged", "inotifyIntact", "accountSnapshotsAgree", "noncesSettled",
                   "nativePass", "receiverHistoryRead")
OBSERVER_COUNTS = ("calibrationOpens", "proverOpensInWindow", "proofsGeneratedByObserver", "rawTransactionsBefore",
                   "rawTransactionsAfter", "chainSamples", "chainSampleErrors", "prewindowSnapshotMissing")
OBSERVER_BINDINGS = ("sourceCommit", "runBindingSha256", "configurationSha256", "scriptSha256", "proverSha256", "nativeResultSha256")
OBSERVER_FAILURES = ("none", "observer_result_unavailable", "observer_binding_invalid", "observer_exit_nonzero",
                     "native_exit_nonzero", "observer_conditions_failed")


def observer_bindings(run, expected):
    configuration = regular(run / "configuration.json", 16384)
    config = json.loads(configuration)
    if config["source_commit"] != expected:
        raise ValueError("Observer configuration source differs")
    observer = runpy.run_path(str(ROOT / OBSERVER_SCRIPT))
    native = run / "native/public/native-result.json"
    return {"sourceCommit":expected,"runBindingSha256":observer["run_binding"](run, configuration, ROOT),
        "configurationSha256":hashlib.sha256(configuration).hexdigest(),"scriptSha256":digest(ROOT / OBSERVER_SCRIPT),
        "proverSha256":config["sha256"]["prover"],"nativeResultSha256":hashlib.sha256(regular(native,131072)).hexdigest() if native.is_file() and not native.is_symlink() else None}


def project_observer_summary(value, bindings):
    fields = {*OBSERVER_BINDINGS, *OBSERVER_CHECKS, *OBSERVER_COUNTS,
              "schemaVersion", "scope", "status", "failureCode", "nativeExitCode", "observerExitCode",
              "calibrationExitCode", "windowStarted", "windowEnded"}
    if not isinstance(value,dict) or set(value) != fields or type(value["schemaVersion"]) is not int or value["schemaVersion"] != 1 or value["scope"] != "original-settlement-to-native-cleanup":
        raise ValueError("Invalid observer summary schema")
    if any(value[name] != bindings[name] for name in OBSERVER_BINDINGS):
        raise ValueError("Observer summary binding differs")
    if any(not isinstance(value[name],str) or not re.fullmatch(r"[0-9a-f]{64}",value[name]) for name in OBSERVER_BINDINGS if name not in ("sourceCommit","nativeResultSha256")):
        raise ValueError("Invalid observer digest")
    if value["nativeResultSha256"] is not None and not re.fullmatch(r"[0-9a-f]{64}",value["nativeResultSha256"]):
        raise ValueError("Invalid native result digest")
    if value["status"] not in ("PASS","INCONCLUSIVE") or value["failureCode"] not in OBSERVER_FAILURES:
        raise ValueError("Invalid observer result enum")
    if any(type(value[name]) is not bool for name in (*OBSERVER_CHECKS,"windowStarted","windowEnded")):
        raise ValueError("Invalid observer condition")
    if any(value[name] is not None and (type(value[name]) is not int or not 0 <= value[name] <= 2**63-1) for name in OBSERVER_COUNTS):
        raise ValueError("Invalid observer counter")
    if any(value[name] is not None and (type(value[name]) is not int or not -128 <= value[name] <= 255) for name in ("nativeExitCode","observerExitCode","calibrationExitCode")):
        raise ValueError("Invalid observer exit code")
    passed = (value["failureCode"] == "none" and value["nativeResultSha256"] is not None
        and all(value[name] for name in (*OBSERVER_CHECKS,"windowStarted","windowEnded"))
        and all(value[name] is not None for name in OBSERVER_COUNTS)
        and value["nativeExitCode"] == value["observerExitCode"] == value["calibrationExitCode"] == 0
        and value["proofsGeneratedByObserver"] == value["proverOpensInWindow"] == 0
        and value["calibrationOpens"] is not None and value["calibrationOpens"] > 0
        and value["rawTransactionsBefore"] is not None and value["rawTransactionsBefore"] >= 7
        and value["rawTransactionsAfter"] is not None and value["rawTransactionsAfter"] >= 7
        and value["rawTransactionsBefore"] == value["rawTransactionsAfter"]
        and value["chainSamples"] is not None and value["chainSamples"] >= 2)
    if (value["status"] == "PASS") != passed:
        raise ValueError("Observer status contradicts its certified conditions")
    return {name:value[name] for name in sorted(fields)}, passed


def write_observer_summary(run, expected, native_exit, observer_exit):
    bindings = observer_bindings(run, expected)
    value = {**bindings,"schemaVersion":1,"scope":"original-settlement-to-native-cleanup","status":"INCONCLUSIVE",
        "failureCode":"observer_result_unavailable","nativeExitCode":native_exit,"observerExitCode":observer_exit,
        "calibrationExitCode":None,"windowStarted":False,"windowEnded":False,
        **{name:False for name in OBSERVER_CHECKS}, **{name:None for name in OBSERVER_COUNTS}}
    try:
        raw = json.loads(regular(run / "restart-observer/result.json",131072))
        pairs = {"sourceCommit":"source_commit","runBindingSha256":"run_binding_sha256",
            "configurationSha256":"configuration_sha256","scriptSha256":"script_sha256",
            "proverSha256":"prover_sha256","nativeResultSha256":"native_result_sha256"}
        if any(raw.get(field) != bindings[name] for name,field in pairs.items()):
            value["failureCode"] = "observer_binding_invalid"
        else:
            start,end = raw.get("window_start",{}),raw.get("window_end",{})
            before,after = start.get("rpc_baseline",{}),end.get("rpc_final",{})
            first,last = raw.get("first_anchored_snapshot_in_window") or {},raw.get("last_anchored_snapshot_in_window") or {}
            counts_before,counts_after = before["rpc_counts"],after["rpc_counts"]
            if any(not isinstance(counts,dict) or "eth_sendRawTransaction" not in counts or any(
                    type(item) is not int or item < 0 for item in counts.values()) for counts in (counts_before,counts_after)):
                raise ValueError("Invalid cumulative RPC counters")
            counters = set(counts_before) | set(counts_after)
            accounts = first.get("accounts",{})
            calibration = raw.get("calibration",{})
            delta = raw.get("write_rpc_delta")
            observed_delta = {key:counts_after.get(key,0)-counts_before.get(key,0) for key in counters}
            write_delta = {key:item for key,item in observed_delta.items() if key.startswith(("eth_send","eth_sign","personal_","evm_","miner_","debug_"))}
            native = json.loads(regular(run / "native/public/native-result.json",131072))
            value.update(windowStarted=raw.get("window_started") is True,windowEnded=raw.get("window_ended") is True,
                calibrationExitCode=calibration.get("exit_code"),calibrationOpens=calibration.get("observed_inode_open_events"),
                proverOpensInWindow=raw.get("prover_opens_in_window"),proofsGeneratedByObserver=raw.get("proofs_generated_by_observer"),
                rawTransactionsBefore=counts_before.get("eth_sendRawTransaction"),rawTransactionsAfter=counts_after.get("eth_sendRawTransaction"),
                chainSamples=raw.get("chain_samples"),chainSampleErrors=raw.get("chain_sample_errors"),
                prewindowSnapshotMissing=raw.get("prewindow_rpc_snapshot_missing",{}).get("count"),
                counterReadyToStopped=before.get("state")=="ready" and after.get("state")=="stopped",
                otherMethodsZero=all(type(snapshot.get("other_logged_methods")) is int and snapshot["other_logged_methods"]==0 for snapshot in (before,after)),
                counterNondecreasing=all(item>=0 for item in observed_delta.values()),
                writeRpcZero=isinstance(delta,dict) and all(type(item) is int for item in delta.values())
                    and delta==write_delta and all(item==0 for item in write_delta.values()),
                proverIdentityUnchanged=raw.get("prover_inode_and_identity_unchanged") is True,
                inotifyIntact=raw.get("flags")==[] and calibration.get("status")=="PASS",
                accountSnapshotsAgree=bool(accounts) and accounts==last.get("accounts"),
                noncesSettled=bool(accounts) and all(item["nonce"]==item["pending_nonce"] for item in accounts.values()),
                nativePass=raw.get("native_status")==native.get("status")=="PASS",
                receiverHistoryRead=(raw.get("native_real_process_restart") or {}).get("receiver_history_read") is True
                    and (native.get("real_process_restart") or {}).get("receiver_history_read") is True)
            value["failureCode"] = ("observer_exit_nonzero" if observer_exit != 0 else "native_exit_nonzero" if native_exit != 0
                else "none" if raw.get("status")=="PASS" else "observer_conditions_failed")
            candidate = {**value,"status":"PASS"}
            try: project_observer_summary(candidate,bindings)
            except (ValueError,TypeError):
                if value["failureCode"] == "none": value["failureCode"] = "observer_conditions_failed"
            else: value = candidate
    except (OSError,ValueError,TypeError,KeyError,AttributeError):
        value.update(status="INCONCLUSIVE",failureCode="observer_result_unavailable")
        for name in OBSERVER_COUNTS: value[name] = None
        value["calibrationExitCode"] = None
    for name in (*OBSERVER_COUNTS,"calibrationExitCode"):
        item = value[name]
        maximum = 255 if name == "calibrationExitCode" else 2**63-1
        minimum = -128 if name == "calibrationExitCode" else 0
        if item is not None and (type(item) is not int or not minimum <= item <= maximum):
            value[name] = None
            value.update(status="INCONCLUSIVE",failureCode="observer_conditions_failed")
    value, passed = project_observer_summary(value,bindings)
    path = run / "restart-observer-summary.json"
    write_private(path,value)
    return {"summary":"restart-observer-summary.json","sha256":digest(path),
        "nativeExitCode":native_exit,"observerExitCode":observer_exit}, passed


def supervise_protected(configuration, run, environment, log):
    ready_reader,ready_writer = os.pipe()
    try: done_reader,done_writer = os.pipe()
    except BaseException:
        os.close(ready_reader); os.close(ready_writer)
        raise
    owned = {ready_reader,ready_writer,done_reader,done_writer}
    native = observer = None
    native_exit = observer_exit = None
    interrupted = False
    pending_signal = None
    failed = False
    previous = {action:signal.getsignal(action) for action in (signal.SIGINT,signal.SIGTERM)}
    def close(descriptor):
        if descriptor in owned:
            owned.remove(descriptor); os.close(descriptor)
    def interrupt(signum,_frame):
        nonlocal interrupted,pending_signal
        interrupted = True
        if native is not None and native.poll() is None:
            native.send_signal(signum)
        elif native is None:
            pending_signal = signum
    for action in previous: signal.signal(action,interrupt)
    try:
        with log.open("ab") as output:
            child_env = {key:value for key,value in environment.items() if key not in (OBSERVER_READY_FD,NATIVE_DONE_FD)}
            observer = subprocess.Popen([sys.executable,str(ROOT / OBSERVER_SCRIPT),"--configuration",str(configuration),"--run-dir",str(run)],
                stdin=subprocess.DEVNULL,stdout=output,stderr=output,start_new_session=True,
                env={**child_env,OBSERVER_READY_FD:str(ready_writer),NATIVE_DONE_FD:str(done_reader)},pass_fds=(ready_writer,done_reader))
            close(ready_writer); close(done_reader)
            if interrupted: raise KeyboardInterrupt()
            native = subprocess.Popen([sys.executable,str(ROOT / "scripts/run-delivery-live.py"),"--protected-config",str(configuration),
                "--protected-run-dir",str(run / "native")],stdin=subprocess.DEVNULL,start_new_session=True,
                env={**child_env,OBSERVER_READY_FD:str(ready_reader)},pass_fds=(ready_reader,))
            close(ready_reader)
            if pending_signal is not None and native.poll() is None:
                native.send_signal(pending_signal)
            while native_exit is None or observer_exit is None:
                if native_exit is None and native.poll() is not None:
                    native_exit = native.wait(); close(done_writer)
                if observer_exit is None and observer.poll() is not None:
                    observer_exit = observer.wait()
                    if native_exit is None and (observer_exit != 0 or not (run / "native/public/native-result.json").is_file()):
                        native.send_signal(signal.SIGTERM)
                if native_exit is None or observer_exit is None: time.sleep(0.02)
    except BaseException:
        failed = True
    finally:
        try:
            if native is not None and native_exit is None:
                if native.poll() is None: native.send_signal(signal.SIGTERM)
                native_exit = native.wait()
            close(done_writer)
            for descriptor in tuple(owned): close(descriptor)
            if observer is not None and observer_exit is None: observer_exit = observer.wait()
        finally:
            for action,handler in previous.items(): signal.signal(action,handler)
    record,passed = write_observer_summary(run,json.loads(regular(configuration,16384))["source_commit"],native_exit,observer_exit)
    return record, passed and not interrupted and not failed


def self_test_observer_fixture(run, expected, native_extra=None):
    configuration = run / "configuration.json"
    write_private(configuration,{"source_commit":expected,"sha256":{"prover":"b"*64}})
    terminal = run / "native/public/native-result.json"
    terminal.parent.mkdir(parents=True,exist_ok=True)
    terminal.write_text(json.dumps({"status":"PASS","real_process_restart":{"receiver_history_read":True}, **(native_extra or {})}))
    bindings = observer_bindings(run,expected)
    statistics = {"rpc_counts":{"eth_sendRawTransaction":7},"other_logged_methods":0}
    accounts = {name:{"nonce":"0x1","pending_nonce":"0x1","private":"UNAPPROVED_PRIVATE_SENTINEL"} for name in ("S","R","P")}
    raw = {"source_commit":expected,"run_binding_sha256":bindings["runBindingSha256"],
        "configuration_sha256":bindings["configurationSha256"],"script_sha256":bindings["scriptSha256"],
        "prover_sha256":bindings["proverSha256"],"native_result_sha256":bindings["nativeResultSha256"],
        "status":"PASS","window_started":True,"window_ended":True,"prover_opens_in_window":0,"proofs_generated_by_observer":0,
        "calibration":{"status":"PASS","exit_code":0,"observed_inode_open_events":1},"flags":[],
        "window_start":{"rpc_baseline":{**statistics,"state":"ready"}},"window_end":{"rpc_final":{**statistics,"state":"stopped"}},
        "write_rpc_delta":{"eth_sendRawTransaction":0},"prover_inode_and_identity_unchanged":True,
        "first_anchored_snapshot_in_window":{"accounts":accounts},"last_anchored_snapshot_in_window":{"accounts":accounts},
        "chain_samples":2,"chain_sample_errors":0,"prewindow_rpc_snapshot_missing":{"count":0},
        "native_status":"PASS","native_real_process_restart":{"receiver_history_read":True}}
    (run / "restart-observer").mkdir()
    write_private(run / "restart-observer/result.json",raw)
    return configuration


def self_test_supervision():
    from unittest.mock import patch
    expected = "1"*40
    def forbidden(*_args,**_kwargs): raise AssertionError("supervisor self-test started business work")
    scenes = ("native-first","observer-first","observer-exit1","native-exit1","early-observer-zero",
              "observer-spawn-failure","native-spawn-failure","interrupt","interrupt-during-native-spawn","native-marker-exit1")
    for scene in scenes:
        with tempfile.TemporaryDirectory(prefix="protected-supervisor-selftest-",dir=ARTIFACTS) as directory:
            run = Path(directory); configuration = self_test_observer_fixture(run,expected)
            terminal = run / "native/public/native-result.json"
            marker = terminal.read_bytes()
            if scene == "early-observer-zero": terminal.unlink()
            tick,children,timeline = [0],{},[]
            handlers = {action:signal.getsignal(action) for action in (signal.SIGINT,signal.SIGTERM)}
            class Child:
                def __init__(self,kind,options):
                    self.kind=kind; self.copies={fd:os.dup(fd) for fd in options["pass_fds"]}; self.forced=False; self.reaped=False
                    self.environment=options["env"]
                    assert all(not os.get_inheritable(fd) for fd in self.copies.values())
                def poll(self):
                    if self.kind=="native":
                        if self.forced or tick[0]>= (3 if scene in ("observer-first","observer-exit1","early-observer-zero","interrupt","interrupt-during-native-spawn") else 1):
                            if not terminal.exists(): terminal.write_bytes(marker)
                            return 1 if self.forced or scene in ("native-exit1","native-marker-exit1") else 0
                    else:
                        done=self.copies[int(self.environment[NATIVE_DONE_FD])]
                        import select
                        eof=bool(select.select([done],[],[],0)[0])
                        if eof: timeline.append(("done-eof",tick[0]))
                        if tick[0]>= (1 if scene in ("observer-first","observer-exit1","early-observer-zero") else 3) or eof and "native" not in children:
                            return 1 if scene=="observer-exit1" or "native" not in children else 0
                    return None
                def wait(self):
                    code=self.poll()
                    assert code is not None, (scene,self.kind,"reaped before actual exit")
                    timeline.append(("wait",self.kind,tick[0])); self.reaped=True
                    for fd in self.copies.values(): os.close(fd)
                    self.copies.clear()
                    return code
                def send_signal(self,signum):
                    assert self.kind=="native", "observer was signalled before its final save"
                    timeline.append(("signal",signum)); self.forced=True
            def spawn(arguments,**options):
                kind="observer" if OBSERVER_SCRIPT in arguments[1] else "native"
                if scene==kind+"-spawn-failure": raise OSError("synthetic spawn failure")
                assert not any("--delivery"==part for part in arguments)
                if kind=="observer":
                    assert set(options["pass_fds"])=={int(options["env"][OBSERVER_READY_FD]),int(options["env"][NATIVE_DONE_FD])}
                else:
                    assert "--protected-config" in arguments and NATIVE_DONE_FD not in options["env"]
                    assert options["pass_fds"]==(int(options["env"][OBSERVER_READY_FD]),)
                    if scene=="interrupt-during-native-spawn": signal.getsignal(signal.SIGINT)(signal.SIGINT,None)
                children[kind]=Child(kind,options); return children[kind]
            def sleep(_duration):
                tick[0]+=1
                assert tick[0]<12, "supervisor did not converge after child exits"
                if scene=="interrupt" and tick[0]==1:
                    signal.getsignal(signal.SIGINT)(signal.SIGINT,None)
            actual_write=write_observer_summary
            def write(*args):
                assert all(child.reaped for child in children.values()), "summary certified a process still running"
                timeline.append(("summary",tick[0])); return actual_write(*args)
            with patch.object(subprocess,"Popen",side_effect=spawn), patch.object(subprocess,"run",forbidden), \
                 patch.object(socket,"socket",forbidden), patch.object(time,"sleep",side_effect=sleep), \
                 patch.dict(globals(),write_observer_summary=write):
                record,passed=supervise_protected(configuration,run,{"TOKIO_WORKER_THREADS":"2"},run / "private.log")
            assert passed == (scene in ("native-first","observer-first")), (scene,record,passed)
            assert all(signal.getsignal(action)==handler for action,handler in handlers.items())
            assert all(child.reaped for child in children.values())
            if scene=="native-first":
                assert record["nativeExitCode"]==record["observerExitCode"]==0
                assert next(item[2] for item in timeline if item[:2]==("wait","native")) < next(item[2] for item in timeline if item[:2]==("wait","observer"))
                assert any(item[0]=="done-eof" for item in timeline)
            if scene in ("observer-exit1","early-observer-zero","interrupt","interrupt-during-native-spawn"):
                assert any(item[0]=="signal" for item in timeline), (scene,timeline)
    print("Protected supervisor self-tests passed; child processes and sockets were mocked")


def protected_selfcheck():
    driver = runpy.run_path(str(ROOT / "scripts/verify-node.py"))
    driver["delivery_protected_selfcheck"]()
    runpy.run_path(str(ROOT / OBSERVER_SCRIPT))["self_test"]()
    self_test_supervision()
    self_test_observer_summary()


def self_test_observer_summary():
    from copy import deepcopy
    expected = "1"*40
    with tempfile.TemporaryDirectory(prefix="protected-observer-summary-selftest-",dir=ARTIFACTS) as directory:
        run=Path(directory); self_test_observer_fixture(run,expected)
        manifest,passed=write_observer_summary(run,expected,0,0)
        assert passed
        target=run / manifest["summary"]; positive=json.loads(target.read_bytes()); bindings=observer_bindings(run,expected)
        assert b"UNAPPROVED_PRIVATE_SENTINEL" not in target.read_bytes()
        changes = [{"unapproved":"UNAPPROVED_PRIVATE_SENTINEL"},{"status":"UNKNOWN"},{"failureCode":"UNAPPROVED_PRIVATE_SENTINEL"},
                   {"schemaVersion":True},{"rawTransactionsBefore":True},{"nativeExitCode":1},{"observerExitCode":1},
                   {"chainSamples":1},{"calibrationOpens":0},{"proverOpensInWindow":1},{"counterReadyToStopped":False}]
        changes += [{name:"0"*64} for name in OBSERVER_BINDINGS]
        changes += [{name:None} for name in OBSERVER_COUNTS]
        changes += [{"rawTransactionsAfter":8}]
        for change in changes:
            invalid={**deepcopy(positive),**change}
            try: project_observer_summary(invalid,bindings)
            except (ValueError,TypeError): pass
            else: raise AssertionError("an invalid observer summary was certified")
        for native_exit,observer_exit in ((1,0),(0,1),(None,None)):
            target.unlink()
            manifest,passed=write_observer_summary(run,expected,native_exit,observer_exit)
            assert not passed and json.loads(target.read_bytes())["status"]=="INCONCLUSIVE"
        raw_path=run / "restart-observer/result.json"; raw=json.loads(raw_path.read_bytes())
        for change in ({"script_sha256":"0"*64},{"window_started":False},{"calibration":{"exit_code":"UNAPPROVED_PRIVATE_SENTINEL"}},
                       {"write_rpc_delta":{"eth_sendRawTransaction":1}}, {"status":"INCONCLUSIVE"}):
            raw_path.write_text(json.dumps({**raw,**change})); target.unlink()
            _,passed=write_observer_summary(run,expected,0,0)
            assert not passed and b"UNAPPROVED_PRIVATE_SENTINEL" not in target.read_bytes()
        contradictory=[]
        for method in ("eth_sendRawTransaction","eth_sign","personal_sign","evm_mine","eth_sendTransaction",
                       "eth_signTypedData","personal_unlockAccount","evm_setTime","miner_start","debug_traceTransaction"):
            changed=deepcopy(raw)
            counts=changed["window_end"]["rpc_final"]["rpc_counts"]
            counts[method]=counts.get(method,0)+1
            contradictory.append(("unreported-"+method,changed))
            changed=deepcopy(changed); changed["write_rpc_delta"][method]=1
            contradictory.append(("reported-"+method,changed))
        for declaration in ({},{"eth_sendRawTransaction":False},{"eth_sendRawTransaction":0.0},
                            {"eth_sendRawTransaction":0,"personal_sign":0}):
            contradictory.append(("inexact-or-mismatched-declaration",{**deepcopy(raw),"write_rpc_delta":declaration}))
        changed=deepcopy(raw)
        for snapshot in (changed["window_start"]["rpc_baseline"],changed["window_end"]["rpc_final"]):
            snapshot["rpc_counts"]["personal_sign"]=0
        contradictory.append(("omitted-zero-write-method",changed))
        for boundary in ("window_start","window_end"):
            field="rpc_baseline" if boundary=="window_start" else "rpc_final"
            for invalid in (None,[],{}, {"eth_call":1}, {"eth_sendRawTransaction":7,"eth_call":True},
                            {"eth_sendRawTransaction":7,"eth_call":-1}, {"eth_sendRawTransaction":7,"eth_call":1.0},
                            {"eth_sendRawTransaction":7,"eth_call":"1"}, {"eth_sendRawTransaction":7,"eth_call":None}):
                changed=deepcopy(raw); changed[boundary][field]["rpc_counts"]=invalid
                contradictory.append(("invalid-counter-map-"+boundary,changed))
            changed=deepcopy(raw); del changed[boundary][field]["rpc_counts"]
            contradictory.append(("missing-counter-map-"+boundary,changed))
            changed=deepcopy(raw); changed[boundary][field]["other_logged_methods"]=False
            contradictory.append(("inexact-other-method-count-"+boundary,changed))
        for field in ("chain_samples","chain_sample_errors"):
            for invalid in (None,True,-1,"0"):
                changed=deepcopy(raw); changed[field]=invalid
                contradictory.append(("invalid-"+field,changed))
            changed=deepcopy(raw); del changed[field]
            contradictory.append(("missing-"+field,changed))
        for invalid in (None,False,-1,"0"):
            changed=deepcopy(raw); changed["prewindow_rpc_snapshot_missing"]["count"]=invalid
            contradictory.append(("invalid-prewindow-missing-count",changed))
        changed=deepcopy(raw); del changed["prewindow_rpc_snapshot_missing"]["count"]
        contradictory.append(("missing-prewindow-missing-count",changed))
        for name,changed in contradictory:
            raw_path.write_text(json.dumps(changed)); target.unlink()
            _,passed=write_observer_summary(run,expected,0,0)
            assert not passed and json.loads(target.read_bytes())["status"]=="INCONCLUSIVE",name
        restored=deepcopy(raw)
        restored["chain_sample_errors"]=3; restored["prewindow_rpc_snapshot_missing"]["count"]=1
        restored["window_end"]["rpc_final"]["rpc_counts"]["eth_coinbase"]=2
        raw_path.write_text(json.dumps(restored)); target.unlink()
        _,passed=write_observer_summary(run,expected,0,0)
        result=json.loads(target.read_bytes())
        assert passed and result["chainSampleErrors"]==3 and result["prewindowSnapshotMissing"]==1
        sentinel = "UNAPPROVED_PRIVATE_SENTINEL"
        projected = project_wrapper_summary("artifact-provenance.json", {"status":"FAIL","sourceCommit":expected,"raw":sentinel,
            "failureCode":sentinel,"runtimeIdentity":{"uid":123,"raw":sentinel},"guestInfo":{"imageId":"a"*64,"raw":sentinel},
            "restartObserver":{"raw":sentinel}})
        assert sentinel not in json.dumps(projected) and projected["failureCode"]=="OtherFailure"
        projected = project_wrapper_summary("resources.json", {"status":"FAIL","initial":{"memory.current":"1","raw":sentinel},"raw":sentinel})
        assert sentinel not in json.dumps(projected)
    print(f"Protected observer projection self-tests passed; {len(contradictory)} counter/stat contradictions rejected, sample errors>0 retained; raw diagnostics were not exported")


def project_wrapper_summary(name, value):
    if name == "resources.json":
        result = {key:value[key] for key in ("status","initial","final","oomDelta","scope") if key in value}
        for key in ("initial","final"):
            if key in result:
                result[key] = {field:item for field,item in result[key].items() if field in
                    ("memory.max","memory.swap.max","memory.current","memory.events","pids.max","pids.current","memory.peak","pids.peak")}
                if any(not isinstance(item,str) or not re.fullmatch(r"[a-z_0-9 \n]+",item) for item in result[key].values()):
                    raise ValueError("Invalid resource counter")
        if "oomDelta" in result:
            result["oomDelta"] = {key:item for key,item in result["oomDelta"].items() if key in ("oom","oom_kill","oom_group_kill")}
            if any(type(item) is not int or item < 0 for item in result["oomDelta"].values()):
                raise ValueError("Invalid OOM delta")
        if "scope" in result and result["scope"] != "entire protected sandbox including artifact preparation":
            raise ValueError("Invalid resource scope")
        return result
    result = {key:value[key] for key in ("schemaVersion","sourceCommit","status","runtimeIdentity","package",
        "packageRuntime","testedBinaryOrigin","testedArtifactSha256","guestInfo","failureCode") if key in value}
    if "schemaVersion" in result and (type(result["schemaVersion"]) is not int or result["schemaVersion"] != 1):
        raise ValueError("Invalid provenance schema")
    if "failureCode" in result and result["failureCode"] not in ("RuntimeError","ValueError","KeyboardInterrupt","SystemExit",
            "OSError","FileNotFoundError","PermissionError","CalledProcessError","TimeoutExpired","JSONDecodeError"):
        result["failureCode"] = "OtherFailure"
    if "runtimeIdentity" in result:
        integers = ("uid","gid","runDirectoryUid")
        booleans = ("rustupReadable","rustupWritable")
        result["runtimeIdentity"] = {key:item for key,item in result["runtimeIdentity"].items() if key in (*integers,*booleans)}
        if any(type(item) is not int or item < 0 for key,item in result["runtimeIdentity"].items() if key in integers) or any(
                type(item) is not bool for key,item in result["runtimeIdentity"].items() if key in booleans):
            raise ValueError("Invalid runtime identity")
    if "package" in result:
        package = result["package"]
        result["package"] = {key:package[key] for key in ("buildInfo","binarySha256","coreSha256","cryptoSha256","wasmSha256") if key in package}
        if "buildInfo" in package:
            verify_build_info(package["buildInfo"])
            result["package"]["buildInfo"] = {key:package["buildInfo"][key] for key in ("version","target","source_commit","update_protocol",
                "data_epoch","core_version","core_source_commit","app_version","app_source_commit") if key in package["buildInfo"]}
        if any(not isinstance(item,str) or not re.fullmatch(r"[0-9a-f]{64}",item) for key,item in result["package"].items() if key != "buildInfo"):
            raise ValueError("Invalid package digest")
    if "testedArtifactSha256" in result:
        result["testedArtifactSha256"] = {key:item for key,item in result["testedArtifactSha256"].items() if key in
            ("binary","core_binary","crypto_module","prover","r0vm","crypto_wasm","profile")}
        if any(not isinstance(item,str) or not re.fullmatch(r"[0-9a-f]{64}",item) for item in result["testedArtifactSha256"].values()):
            raise ValueError("Invalid tested artifact digest")
    if "guestInfo" in result:
        result["guestInfo"] = {key:item for key,item in result["guestInfo"].items() if key in ("imageId","schemaHash","elfSha256","statementBytes")}
        if any(not isinstance(item,str) or not re.fullmatch(r"[0-9a-f]{64}",item) for key,item in result["guestInfo"].items() if key != "statementBytes"):
            raise ValueError("Invalid guest identity")
        if "statementBytes" in result["guestInfo"] and result["guestInfo"]["statementBytes"] != 1504:
            raise ValueError("Invalid guest statement size")
    if "packageRuntime" in result and result["packageRuntime"] not in ("PASS","GLIBC_INCOMPATIBLE"):
        raise ValueError("Invalid package runtime")
    if "testedBinaryOrigin" in result and result["testedBinaryOrigin"] not in ("exact-head Linux package", "clean exact-head source rebuild after measured package GLIBC incompatibility"):
        raise ValueError("Invalid binary origin")
    return result


def protected_public(run, expected, returncode):
    source = run / "native/public"
    output = Path(tempfile.mkdtemp(prefix="public-export-", dir=run)); output.chmod(0o755)
    failures = set()
    recognized = {"progress.json"}
    qualified, public_files = [], {}
    progress = None
    try:
        value = json.loads(regular(source / "progress.json", 131072))
        if not isinstance(value, dict) or value.get("source_commit") != expected or value.get("scope") != "fresh-two-order-protected-native-only":
            raise ValueError("invalid source or scope")
        progress = value
    except Exception:
        failures.add("progress_unavailable")
    native_status = progress.get("status") if progress else None
    if native_status not in ("PASS","FAIL","RUNNING","INTERRUPTED"):
        native_status = "UNVERIFIED"; failures.add("invalid_native_status")
    facts = progress.get("genuine_receipts", []) if progress else []
    if not isinstance(facts, list):
        facts = []; failures.add("invalid_proof_manifest")
    if len(facts) > 5: failures.add("excess_proof_manifest")
    seen = set()
    for fact in facts[:16]:
        pair = []
        try:
            relation = fact["relation"]
            if type(relation) is not int or relation not in (1,2,3): raise ValueError("invalid relation")
            if relation == 1:
                basename = "key-binding"
            else:
                order = fact["orderId"]
                if not isinstance(order,str) or not re.fullmatch(r"0x[0-9a-f]{64}",order) or type(fact["windowIndex"]) is not int or fact["windowIndex"] != 0:
                    raise ValueError("invalid slot")
                basename = f"order-{order[2:]}-relation-{relation}-window-0"
            public_name, receipt_name = basename + ".json", basename + ".receipt"
            if fact["public"] != public_name or fact["receipt"] != receipt_name or basename in seen:
                raise ValueError("invalid or duplicate filename")
            recognized.update((public_name, receipt_name))
            if fact.get("official_host_verified") is not True or fact.get("wrong_journal_receipt_comparison_rejected") is not True:
                raise ValueError("unverified proof")
            if any(not isinstance(fact.get(key),str) or not re.fullmatch(r"[0-9a-f]{64}",fact[key]) for key in ("receiptSha256","publicSha256","journalHash")):
                raise ValueError("invalid proof digest")
            receipt_bytes = regular(source / receipt_name,16384)
            metadata_bytes = regular(source / public_name,16384)
            metadata = json.loads(metadata_bytes)
            if hashlib.sha256(receipt_bytes).hexdigest() != fact["receiptSha256"] or hashlib.sha256(metadata_bytes).hexdigest() != fact["publicSha256"] or metadata["journalHash"] != fact["journalHash"]:
                raise ValueError("changed proof pair")
            certified = {key:fact[key] for key in ("relation","public","receipt","receiptSha256","publicSha256","journalHash")}
            certified.update(official_host_verified=True,wrong_journal_receipt_comparison_rejected=True)
            if relation != 1: certified.update(orderId=fact["orderId"],windowIndex=0)
            pair = [output / public_name, output / receipt_name]
            for target,data in zip(pair,(metadata_bytes,receipt_bytes)):
                target.write_bytes(data); target.chmod(0o644)
            seen.add(basename)
            qualified.append(certified)
        except Exception:
            failures.add("invalid_certified_proof")
            for target in pair: target.unlink(missing_ok=True)
    allowed_files = {"before-source-offline.json","confirmed-chain.json","settlement.json","settlement-observation.json","native-result.json","registration-diagnostics.json"}
    diagnostic_binding = None
    proof_diagnostic_binding = None
    certified_files = progress.get("public_files",{}) if progress else {}
    if not isinstance(certified_files,dict):
        certified_files = {}; failures.add("invalid_public_file_manifest")
    for name,checksum in certified_files.items():
        if name not in allowed_files:
            failures.add("unapproved_public_material"); continue
        recognized.add(name)
        try:
            data = regular(source / name,65536 if name == "registration-diagnostics.json" else 131072)
            if not isinstance(checksum,str) or not re.fullmatch(r"[0-9a-f]{64}",checksum) or hashlib.sha256(data).hexdigest() != checksum:
                raise ValueError("changed public report")
            if name == "registration-diagnostics.json":
                driver = runpy.run_path(str(ROOT / "scripts/verify-delivery-protected.py"))
                native_bytes = regular(source / "native-result.json",131072)
                if hashlib.sha256(native_bytes).hexdigest() != certified_files.get("native-result.json"):
                    raise ValueError("Unauthenticated registration diagnostic source")
                native = json.loads(native_bytes)
                if native.get("source_commit") != expected:
                    raise ValueError("Registration diagnostic source differs")
                projected = driver["project_registration_diagnostics"](driver["diagnostic_json"](data),expected,native["artifact_sha256"])
                diagnostic_binding = projected["artifact_sha256"]
            elif name == "native-result.json":
                driver = runpy.run_path(str(ROOT / "scripts/verify-delivery-protected.py"))
                native = driver["diagnostic_json"](data)
                if native.get("proof_diagnostics") is not None:
                    if native.get("source_commit") != expected:
                        raise ValueError("Proof diagnostic source differs")
                    projected = driver["project_proof_diagnostics"](native["proof_diagnostics"], expected, native["artifact_sha256"],
                        native.get("profile_id"), native.get("principals", {}), native.get("orders", []), qualified)
                    if projected["profile_id"] is not None:
                        profile_bytes = regular(ROOT / "crates/delivery/proof/profile.json", 16384)
                        if hashlib.sha256(profile_bytes).hexdigest() != projected["artifact_sha256"]["profile"] \
                                or driver["profile_id"](driver["diagnostic_json"](profile_bytes)) != projected["profile_id"]:
                            raise ValueError("Proof diagnostic profile differs from the final source")
                    proof_diagnostic_binding = (native, projected)
            else:
                json.loads(data)
            (output / name).write_bytes(data); (output / name).chmod(0o644)
            public_files[name] = checksum
        except Exception:
            failures.add("invalid_certified_public_file")
            (output / name).unlink(missing_ok=True)
    try:
        if any(path.name not in recognized or path.is_symlink() or not path.is_file() for path in source.iterdir()):
            failures.add("unapproved_public_material")
    except Exception:
        failures.add("public_source_unavailable")
    states, wrappers = {}, {}
    for name in ("artifact-provenance.json","resources.json"):
        try:
            data = regular(run / name,131072); value = json.loads(data)
            if not isinstance(value,dict) or value.get("status") not in ("PASS","FAIL","UNVERIFIED") or name == "artifact-provenance.json" and value.get("sourceCommit") != expected:
                raise ValueError("invalid wrapper report")
            if name == "artifact-provenance.json":
                public_wrapper = project_wrapper_summary(name,value)
                replace_public(output / name,public_wrapper)
            else:
                replace_public(output / name,project_wrapper_summary(name,value))
            states[name] = value["status"]
            wrappers[name] = value
        except Exception:
            failures.add("wrapper_report_unavailable")
    if diagnostic_binding is not None:
        try:
            tested = wrappers["artifact-provenance.json"]["testedArtifactSha256"]
            if diagnostic_binding != {key:tested[key] for key in ("prover","r0vm","profile")}:
                raise ValueError("Registration diagnostic artifacts differ")
        except (KeyError,TypeError,ValueError):
            failures.add("invalid_registration_diagnostic_binding")
            (output / "registration-diagnostics.json").unlink(missing_ok=True)
            public_files.pop("registration-diagnostics.json",None)
    if proof_diagnostic_binding is not None:
        try:
            native, diagnostic = proof_diagnostic_binding
            provenance = wrappers["artifact-provenance.json"]
            tested = provenance["testedArtifactSha256"]
            names = {"binary", "core_binary", "crypto_module", "crypto_wasm", "prover", "r0vm", "profile"}
            if set(native["artifact_sha256"]) != names or native["artifact_sha256"] != {name: tested[name] for name in names}:
                raise ValueError("Proof diagnostics differ from the tested artifacts")
            if diagnostic["availability"] != "unavailable":
                if native.get("prover_info") != provenance["guestInfo"]:
                    raise ValueError("Proof diagnostics differ from the tested guest")
                for fact in qualified:
                    metadata = json.loads(regular(output / fact["public"], 16384))
                    if metadata.get("profileId") != diagnostic["profile_id"] or metadata.get("imageId") != provenance["guestInfo"]["imageId"]:
                        raise ValueError("Proof diagnostic receipt profile differs")
        except (KeyError, TypeError, ValueError):
            failures.add("invalid_certified_public_file")
            (output / "native-result.json").unlink(missing_ok=True)
            public_files.pop("native-result.json", None)
    observer_summary = None
    try:
        manifest = wrappers["artifact-provenance.json"]["restartObserver"]
        if set(manifest) != {"summary","sha256","nativeExitCode","observerExitCode"} or manifest["summary"] != "restart-observer-summary.json":
            raise ValueError("Invalid observer manifest")
        data = regular(run / "restart-observer-summary.json",16384)
        if hashlib.sha256(data).hexdigest() != manifest["sha256"]:
            raise ValueError("Changed observer summary")
        candidate,observer_passed = project_observer_summary(json.loads(data),observer_bindings(run,expected))
        if any(candidate[name] != manifest[name] for name in ("nativeExitCode","observerExitCode")):
            raise ValueError("Observer exit authentication differs")
        observer_summary = candidate
        replace_public(output / "restart-observer-summary.json",observer_summary)
        public_wrapper["restartObserver"] = {"summary":"restart-observer-summary.json","sha256":digest(output / "restart-observer-summary.json"),
            "nativeExitCode":observer_summary["nativeExitCode"],"observerExitCode":observer_summary["observerExitCode"]}
        replace_public(output / "artifact-provenance.json",public_wrapper)
        if not observer_passed: failures.add("restart_observer_not_passed")
    except Exception:
        failures.add("restart_observer_unavailable")
    sources = {fact["orderId"] for fact in qualified if fact["relation"] == 2}
    holds = {fact["orderId"] for fact in qualified if fact["relation"] == 3}
    complete = len(qualified) == 5 and sum(fact["relation"] == 1 for fact in qualified) == 1 and len(sources) == 2 and sources == holds
    if not complete: failures.add("incomplete_verified_receipts")
    if "native-result.json" not in public_files: failures.add("native_result_unavailable")
    if native_status != "PASS": failures.add("native_run_not_passed")
    if returncode != 0: failures.add("container_exit_nonzero")
    if any(states.get(name) != "PASS" for name in ("artifact-provenance.json","resources.json")): failures.add("wrapper_checks_not_passed")
    passed = not failures
    if progress is not None:
        replace_public(output / "progress.json", {"schema_version":1,"source_commit":expected,"scope":"fresh-two-order-protected-native-only",
            "status":"PASS" if passed else "FAIL","genuine_receipts":qualified,"public_files":public_files,"native_status":native_status})
    replace_public(output / "summary.json", {"schemaVersion":1,"status":"PASS" if passed else "FAIL","sourceCommit":expected,
        "scope":"fresh-two-order-protected-native-only","genuineReceipts":len(qualified),"nativeStatus":native_status,"containerExitCode":returncode,
        "restartObserverStatus":observer_summary["status"] if observer_summary else "UNVERIFIED",
        "nativeExitCode":observer_summary["nativeExitCode"] if observer_summary else None,
        "observerExitCode":observer_summary["observerExitCode"] if observer_summary else None,
        "failureCodes":sorted(failures),
        "notChecked":["rendered Web or Android UI","old fee5 identical bytes or maximum capacity","forced reorg/nonce-revert/near-cutoff/token faults","public chain or real assets"]})
    return output, passed


def protected_package_dispatch(package_input, expected, run_directory):
    source_commit(expected)
    if "trash" in str(package_input).lower() or run_directory is not None and "trash" in str(run_directory).lower():
        raise ValueError("Forbidden protected artifact path")
    package_input = package_input.resolve(strict=True)
    if not package_input.is_relative_to(ROOT / ".artifacts"):
        raise ValueError("Protected package inputs stay in this task's .artifacts")
    run = (run_directory or ARTIFACTS / "protected-runs" / uuid.uuid4().hex).resolve()
    if not run.is_relative_to(ROOT / ".artifacts") or run.exists():
        raise ValueError("Protected acceptance requires a fresh task artifact directory")
    run.mkdir(mode=0o700, parents=True)
    package = package_input if (package_input / "BUILD-INFO.json").exists() else unpack_package(package_input, run / "package")
    checked_package(package, expected)
    common = Path(subprocess.check_output(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], cwd=ROOT, text=True).strip()).resolve()
    git_directory = Path(subprocess.check_output(["git", "rev-parse", "--absolute-git-dir"], cwd=ROOT, text=True).strip()).resolve()
    checks = runpy.run_path(str(ROOT / "scripts/check-delivery.py"))
    for name in ("cargo", "risc0", "canonical"): (ARTIFACTS / name).mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="protected-build-context-", dir=ARTIFACTS) as directory:
        context = Path(directory)
        for name in ("crates/delivery/proof/Sandbox.Dockerfile", "crates/delivery/proof/confined-gnark.sh"):
            target = context / name; target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, target)
        build, original = checks["sandbox_commands"](context)
        command("build canonical protected sandbox", build, run / "host-private.log")
    name = "vdp-protected-" + uuid.uuid4().hex
    container = protected_container(original, package, expected, run, common, git_directory, name)
    process = None
    returncode = 1
    previous_term = signal.getsignal(signal.SIGTERM)

    def interrupted(*_):
        raise InterruptedError("Protected native acceptance interrupted")

    try:
        # The docker client runs in its own session, so only this handler lets a terminated host reach the cleanup below.
        signal.signal(signal.SIGTERM, interrupted)
        process = subprocess.Popen(container, cwd=ROOT, start_new_session=True)
        returncode = process.wait()
    finally:
        try:
            if process is not None and process.poll() is None:
                subprocess.run(["docker", "stop", "--time", "20", name], capture_output=True, timeout=30)
            removed = subprocess.run(["docker", "rm", "--force", name], capture_output=True, timeout=20)
            if removed.returncode and not any(text in removed.stderr for text in (b"No such container", b"No such object")):
                returncode = returncode or 1
        except Exception:
            returncode = returncode or 1
        finally:
            try:
                terminate_owned(process)
            except Exception:
                returncode = returncode or 1
            signal.signal(signal.SIGTERM, previous_term)
        try:
            public, passed = protected_public(run, expected, returncode)
        except Exception:
            public, passed = protected_public_failure(run, expected, returncode or 1)
        if os.environ.get("GITHUB_OUTPUT"):
            with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output: output.write("public-directory=" + str(public) + "\n")
        print(json.dumps({"event":"protected_native_result","status":"PASS" if passed else "FAIL","public_directory":str(public),"source_commit":expected}), flush=True)
    if not passed:
        raise RuntimeError("Protected native acceptance failed; verified public parts and private task diagnostics were retained")


def protected_package_inside(package, expected, run):
    os.umask(0o077)
    if any("trash" in str(path).lower() for path in (package, run)) or not package.resolve().is_relative_to(ROOT / ".artifacts"):
        raise ValueError("Forbidden protected artifact path")
    source_commit(expected)
    if os.environ.get("TOKIO_WORKER_THREADS") != "2" or any(name.startswith(("RISC0_DEV", "RISC0_SKIP")) for name in os.environ):
        raise ValueError("Protected acceptance requires two Tokio workers and the official proof mode")
    if not run.resolve().is_relative_to(ROOT / ".artifacts") or not run.is_dir() or (run / "native").exists():
        raise ValueError("Protected sandbox requires its fresh task run directory")
    _, initial = resources()
    provenance = {"schemaVersion":1,"sourceCommit":expected,"status":"FAIL",
        "runtimeIdentity":{"uid":os.getuid(),"gid":os.getgid(),"runDirectoryUid":run.stat().st_uid,
            "rustupReadable":os.access(os.environ.get("RUSTUP_HOME","/usr/local/rustup"),os.R_OK),
            "rustupWritable":os.access(os.environ.get("RUSTUP_HOME","/usr/local/rustup"),os.W_OK)}}
    failure = None
    os.environ["npm_config_cache"] = str(run / "npm-cache")
    environment = {**os.environ, "CARGO_BUILD_JOBS":"2", "TOKIO_WORKER_THREADS":"2", "VIBEFACE_APP_DIR":str(package / "web")}
    log = run / "sandbox-private.log"
    previous = signal.getsignal(signal.SIGTERM)
    def interrupt(_signum, _frame): raise KeyboardInterrupt()
    signal.signal(signal.SIGTERM, interrupt)
    try:
        for component in ("crypto", "web"): command("verify " + component + " inputs", ["python3","scripts/build-inputs.py","check",component], log, env=environment)
        info = checked_package(package, expected)
        binary, core, module = package / "vibeface", package / "vibeface-core", package / "web/vibeface_secure.js"
        provenance["package"] = {"buildInfo":info,"binarySha256":digest(binary),"coreSha256":digest(core),"cryptoSha256":digest(module),"wasmSha256":digest(module.with_name("vibeface_secure_bg.wasm"))}
        packaged_runtime = runtime(binary, module, run / "packaged-runtime", log, environment)
        provenance["packageRuntime"] = "PASS" if packaged_runtime else "GLIBC_INCOMPATIBLE"
        provenance["testedBinaryOrigin"] = "exact-head Linux package"
        if not packaged_runtime:
            flags = subprocess.check_output(["python3","scripts/release-rustflags.py"], cwd=ROOT, env={**environment,"CARGO_TARGET_DIR":"/artifacts/node-target"}, text=True).strip()
            command("rebuild current CLI/Core after measured GLIBC incompatibility", ["cargo","+1.98.1","build","--release","--locked","-p","vibeface","-p","vibeface-core","--target-dir","/artifacts/node-target"], log,
                env={**environment,"CARGO_ENCODED_RUSTFLAGS":flags})
            binary, core = Path("/artifacts/node-target/release/vibeface"), Path("/artifacts/node-target/release/vibeface-core")
            if not runtime(binary, module, run / "rebuilt-runtime", log, environment): raise RuntimeError("Current-source binaries cannot run in the canonical sandbox")
            provenance["testedBinaryOrigin"] = "clean exact-head source rebuild after measured package GLIBC incompatibility"
        command("install fixed guest compiler", ["python3","scripts/install-delivery-toolchain.py"], log, env=environment)
        command("build official current guest and host", ["bash","crates/delivery/proof/build.sh"], log, env=environment)
        prover, r0vm = Path("/artifacts/proof-target/release/vibeface-delivery-prover"), Path("/opt/vdp/bin/r0vm")
        compiled = json.loads(subprocess.check_output([str(prover),"info"], cwd=ROOT, env=environment, timeout=30))
        runpy.run_path(str(ROOT / "scripts/check-delivery.py"))["load_profile"](compiled)
        command("install pinned local contract dependencies", ["node","contracts/delivery/install.mjs"], log, env=environment)
        paths = {"repository":str(ROOT),"binary":str(binary),"core_binary":str(core),"crypto_module":str(module),"prover":str(prover),"r0vm":str(r0vm)}
        hashes = {name:digest(Path(path)) for name,path in paths.items() if name != "repository"}
        hashes.update(crypto_wasm=digest(module.with_name("vibeface_secure_bg.wasm")),profile=digest(ROOT / "crates/delivery/proof/profile.json"))
        driver = runpy.run_path(str(ROOT / "scripts/verify-delivery-protected.py"))
        configuration = driver["validate_config"]({"schema_version":1,"source_commit":expected,"paths":paths,"sha256":hashes,"elf_sha256":compiled["elfSha256"],
            "caps":{"deposit":"10","max_service_amount":"5","source_gas_cap_wei":driver["CAP"],"provider_gas_cap_wei":driver["CAP"],"register_key_gas_cap_wei":driver["CAP"],"native_funding_wei_each":str(10**19)},"publication_policy":driver["POLICY"]})
        configuration_path = run / "configuration.json"
        write_private(configuration_path, configuration)
        provenance.update(testedArtifactSha256=hashes,guestInfo=compiled)
        os.environ["VIBEFACE_APP_DIR"] = str(package / "web")
        observer_record,observer_passed = supervise_protected(configuration_path,run,environment,log)
        provenance["restartObserver"] = observer_record
        if not observer_passed:
            raise RuntimeError("Native acceptance or its complete restart observation failed")
        source_commit(expected)
        provenance["status"] = "PASS"
    except BaseException as error:
        failure = error; provenance["failureCode"] = type(error).__name__
    finally:
        try:
            _, final = resources(); delta = oom_delta(initial, final)
            resource = {"status":"PASS" if not any(delta.values()) else "FAIL","initial":initial,"final":final,"oomDelta":delta,"scope":"entire protected sandbox including artifact preparation"}
        except BaseException:
            resource = {"status":"UNVERIFIED","initial":initial}
        if resource["status"] != "PASS": provenance["status"] = "FAIL"
        write_private(run / "artifact-provenance.json", provenance)
        write_private(run / "resources.json", resource)
        signal.signal(signal.SIGTERM, previous)
    if failure is not None or provenance["status"] != "PASS":
        raise RuntimeError("Protected sandbox failed; only independently verified partial evidence may be exported") from None


def protected_dispatch(configuration, run_directory):
    protected = runpy.run_path(str(ROOT / "scripts/verify-delivery-protected.py"))
    if "trash" in str(configuration).lower():
        raise ValueError("Forbidden configuration path")
    config = protected["validate_config"](json.loads(protected["read_regular"](configuration.resolve(strict=True), 16384, private=True)))
    paths = {name: Path(value) for name, value in config["paths"].items()}
    options = argparse.Namespace(delivery_config=configuration, delivery_run_dir=run_directory,
        artifacts=ROOT / ".artifacts", binary=paths["binary"], core_binary=paths["core_binary"], crypto_module=paths["crypto_module"])
    driver = runpy.run_path(str(ROOT / "scripts/verify-node.py"))
    driver["delivery_protected_acceptance"](options)


def self_test_protected(original):
    from copy import deepcopy
    from unittest.mock import Mock, patch
    expected = "1" * 40
    package, run = ROOT / ".artifacts/package", ROOT / ".artifacts/protected-run"
    common = Path("/fixture/repository.git")
    for git_directory in (common, common / "worktrees/task"):
        command_line = protected_container(original, package, expected, run, common, git_directory, "protected-fixture")
        mounts = [command_line[i+1] for i,item in enumerate(command_line) if item == "--mount"]
        assert f"type=bind,source={common},target=/git-common,readonly" in mounts
        assert f"type=bind,source={ROOT / '.git'},target=/work/.git,readonly" in mounts
        environment = dict(command_line[i+1].split("=",1) for i,item in enumerate(command_line) if item == "--env")
        assert environment["GIT_DIR"] == str(Path("/git-common") / git_directory.relative_to(common))
        assert environment["GIT_WORK_TREE"] == "/work" and environment["GIT_OPTIONAL_LOCKS"] == "0"
        assert environment["TOKIO_WORKER_THREADS"] == "2"
        assert "VIBEFACE_DELIVERY_DEADLINE_UNIX_MS" not in environment
        assert command_line[command_line.index("--user")+1] == f"{os.getuid()}:{os.getgid()}"
        assert "--network" not in command_line and "--privileged" not in command_line
        assert "--protected-package" in command_line and "--protected-inside" in command_line
        assert "--android" not in command_line and "--package" not in command_line
        assert command_line[command_line.index("--memory")+1] == command_line[command_line.index("--memory-swap")+1] == "11g"
        assert command_line[command_line.index("--pids-limit")+1] == "64"
    with tempfile.TemporaryDirectory(prefix="protected-stage-bound-selftest-", dir=ARTIFACTS) as temporary:
        package = Path(temporary) / "package"; package.mkdir()
        (package / "BUILD-INFO.json").write_text("{}")
        run = Path(temporary) / "run"
        process = Mock(wait=Mock(return_value=0), poll=Mock(return_value=0))
        def forbidden(*_args, **_kwargs): raise AssertionError("stage-bound selfcheck opened a network socket")
        with patch(f"{__name__}.source_commit"), patch(f"{__name__}.checked_package"), \
             patch(f"{__name__}.command") as build, patch(f"{__name__}.protected_public", return_value=(run / "public", True)), \
             patch("runpy.run_path", return_value={"sandbox_commands": lambda _context: (["synthetic", "build"], original)}), \
             patch("subprocess.check_output", side_effect=[str(common) + "\n", str(common / "worktrees/task") + "\n"]), \
             patch("subprocess.Popen", return_value=process), \
             patch("subprocess.run", return_value=subprocess.CompletedProcess([], 1, b"", b"No such container")), \
             patch("socket.socket", forbidden), patch("time.time", side_effect=[0, 20_701]), \
             patch.dict(os.environ, {"GITHUB_OUTPUT": ""}), patch("builtins.print"):
            protected_package_dispatch(package, expected, run)
            process.wait.assert_called_once_with()
            previous_term = signal.getsignal(signal.SIGTERM)
            terminated = Mock(wait=Mock(side_effect=lambda: os.kill(os.getpid(), signal.SIGTERM) or time.sleep(5)), poll=Mock(side_effect=[None, 0]))
            with patch("subprocess.Popen", return_value=terminated), patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, b"", b"")) as cleanup, \
                 patch("subprocess.check_output", side_effect=[str(common) + "\n", str(common / "worktrees/task") + "\n"]):
                try: protected_package_dispatch(package, expected, Path(temporary) / "terminated")
                except InterruptedError: pass
                else: raise AssertionError("a terminated protected host must not report success")
            assert [call.args[0][:2] for call in cleanup.call_args_list] == [["docker", "stop"], ["docker", "rm"]], "a terminated host must stop its container"
            assert signal.getsignal(signal.SIGTERM) is previous_term
        assert "timeout" not in build.call_args.kwargs, "the native stages must not inherit the old UI total timeout"
    with tempfile.TemporaryDirectory(prefix="protected-npm-cache-selftest-", dir=ARTIFACTS) as temporary:
        run = Path(temporary); package = run / "package"; package.mkdir()
        snapshot = {"memory.events":"oom 0\noom_kill 0\noom_group_kill 0\n"}
        with patch(f"{__name__}.source_commit"), patch(f"{__name__}.resources", return_value=(Path('/synthetic-cgroup'),snapshot)), \
             patch(f"{__name__}.command", side_effect=RuntimeError("synthetic stop before artifact preparation")) as prepare, \
             patch("subprocess.Popen", forbidden), patch("subprocess.run", forbidden), patch("socket.socket", forbidden), \
             patch.dict(os.environ,{"HOME":"/home/runner","TOKIO_WORKER_THREADS":"2","RISC0_HOME":"/risc0", \
                 "RISC0_SERVER_PATH":"/opt/vdp/bin/r0vm","npm_config_cache":"/home/runner/.npm"},clear=True):
            try: protected_package_inside(package,expected,run)
            except RuntimeError as error: assert str(error) == "Protected sandbox failed; only independently verified partial evidence may be exported"
            else: raise AssertionError("the preparation selfcheck must stop before real work")
            prepare.assert_called_once()
            environment = prepare.call_args.kwargs["env"]
            assert environment["npm_config_cache"] == os.environ["npm_config_cache"] == str(run / "npm-cache")
            assert environment["HOME"] == "/home/runner" and environment["RISC0_HOME"] == "/risc0"
            assert environment["RISC0_SERVER_PATH"] == "/opt/vdp/bin/r0vm" and environment["TOKIO_WORKER_THREADS"] == "2"
            assert not any(name.startswith(("RISC0_DEV","RISC0_SKIP")) for name in environment)
    driver = runpy.run_path(str(ROOT / "scripts/verify-node.py"))
    with tempfile.TemporaryDirectory(prefix="protected-public-node-selftest-", dir=ARTIFACTS) as temporary:
        directory = Path(temporary)
        binary = directory / "vibeface"; binary.touch(mode=0o600)
        options = argparse.Namespace(binary=binary, artifacts=directory / "fixtures",
            crypto_module=directory / "vibeface_secure.js", fixture_kind="mesh", fixture_port=27100)
        class FixtureConstructed(BaseException): pass
        mesh = "t-123456789abc"
        public = Mock(device_id="11" * 32, quic_port=27102, directory=directory / "public",
            mesh_address="vibeface://node/" + "11" * 32 + "?addr=127.0.0.1:27102")
        member = Mock(directory=directory / "member")
        for node in (public, member):
            node.directory.mkdir()
            (node.directory / "config.json").write_text('{"bins":{}}')
        public.start.side_effect = FixtureConstructed
        constructor = Mock(side_effect=[public, member])
        cleanup = Mock()
        configure = Mock()
        relay_args = ["--relay-listen", "127.0.0.1:27101", "--relay-url", "http://127.0.0.1:27101",
            "--relay-open", "--relay-rate", "1048576", "--relay-max-clients", "64", "--relay-max-clients-per-ip", "64"]
        with patch.dict(driver["e2e_fixture"].__globals__, Node=constructor, available_port=lambda: 27101,
                        mesh_network=lambda: mesh, node_cli=configure, stop_fixture_processes=cleanup), patch("signal.signal"), \
             patch("subprocess.Popen", forbidden), patch("subprocess.run", forbidden), patch("socket.socket", forbidden):
            assert driver["public_node_args"]() == relay_args
            try: driver["e2e_fixture"](options)
            except FixtureConstructed: pass
            else: raise AssertionError("public Node fixture selfcheck must stop before process startup")
        assert len(constructor.call_args_list) == 2
        public_call, member_call = constructor.call_args_list
        run = public_call.args[0]
        assert public_call.args[1] == "public" and member_call.args[1] == "member"
        assert public_call.kwargs == {"port": options.fixture_port,
            "init_args": ["--mesh-network", mesh, "--forward-any", *relay_args]}
        assert member_call.kwargs == {"init_args": ["--root", str(run / "workspace"), "--mesh-network", mesh,
            "--bootstrap", public.mesh_address]}
        public.configure_mesh.assert_called_once_with(mesh)
        configure.assert_called_once_with(member, "config", "set", "network.mesh.allow_private", "true")
        public.start.assert_called_once_with()
        member.start.assert_not_called()
        for node in (public, member):
            node.client.close.assert_called_once_with()
            node.stop.assert_called_once_with()
        cleanup.assert_called_once_with(run)
    with tempfile.TemporaryDirectory(prefix="protected-export-selftest-", dir=ARTIFACTS) as temporary:
        run = Path(temporary); public = run / "native/public"; public.mkdir(parents=True)
        facts = []
        for relation, order in [(1,None),(2,"ab"*32),(3,"ab"*32),(2,"cd"*32),(3,"cd"*32)]:
            name = "key-binding" if relation == 1 else f"order-{order}-relation-{relation}-window-0"
            receipt = public / (name+".receipt"); receipt.write_bytes(b"synthetic exporter selfcheck only: "+name.encode())
            metadata = public / (name+".json"); metadata.write_text(json.dumps({"journalHash":"a"*64}))
            fact = {"relation":relation,"public":name+".json","receipt":name+".receipt","receiptSha256":digest(receipt),"publicSha256":digest(metadata),"journalHash":"a"*64,
                "official_host_verified":True,"wrong_journal_receipt_comparison_rejected":True}
            if order: fact.update(orderId="0x"+order,windowIndex=0)
            facts.append(fact)
        self_test_observer_fixture(run,expected)
        observer_record,observer_passed = write_observer_summary(run,expected,0,0)
        assert observer_passed
        terminal = public / "native-result.json"
        base = {"source_commit":expected,"scope":"fresh-two-order-protected-native-only","status":"PASS","genuine_receipts":facts,
            "public_files":{"native-result.json":digest(terminal)}}
        (run / "artifact-provenance.json").write_text(json.dumps({"sourceCommit":expected,"status":"PASS","restartObserver":observer_record}))
        (run / "resources.json").write_text(json.dumps({"status":"PASS"}))
        def export(manifest, returncode=0):
            (public / "progress.json").write_text(json.dumps(manifest))
            output, passed = protected_public(run, expected, returncode)
            summary = json.loads((output / "summary.json").read_text())
            assert summary["status"] == ("PASS" if passed else "FAIL")
            assert summary["nativeStatus"] in ("PASS","FAIL","RUNNING","INTERRUPTED","UNVERIFIED")
            assert all(re.fullmatch(r"[a-z_]+",code) for code in summary["failureCodes"])
            assert len(list(output.glob("*.receipt"))) == summary["genuineReceipts"]
            return output, passed, summary
        output, passed, summary = export(base)
        assert passed and summary["genuineReceipts"] == 5
        assert (output / "key-binding.receipt").stat().st_mode & 0o777 == 0o644
        assert json.loads((output / "artifact-provenance.json").read_bytes())["restartObserver"]["sha256"] == digest(output / "restart-observer-summary.json")
        observer_path = run / "restart-observer-summary.json"
        provenance_path = run / "artifact-provenance.json"
        observer_bytes,provenance_bytes = observer_path.read_bytes(),provenance_path.read_bytes()
        observer_value = json.loads(observer_bytes)
        for change in ({"raw_diagnostic":"UNAPPROVED_PRIVATE_SENTINEL"},{"status":"UNKNOWN"},{"observerExitCode":1},
                       {"nativeExitCode":1},{"windowStarted":False},{"calibrationOpens":0},{"rawTransactionsAfter":8},
                       {"chainSampleErrors":None},{"prewindowSnapshotMissing":None},
                       *({name:"0"*64} for name in OBSERVER_BINDINGS)):
            observer_path.write_text(json.dumps({**observer_value,**change}))
            provenance = json.loads(provenance_bytes)
            provenance["restartObserver"]["sha256"] = digest(observer_path)
            provenance_path.write_text(json.dumps(provenance))
            rejected,passed,details = export(base)
            assert not passed and details["genuineReceipts"]==5 and details["restartObserverStatus"]=="UNVERIFIED"
            assert not (rejected / "restart-observer-summary.json").exists()
            assert all(b"UNAPPROVED_PRIVATE_SENTINEL" not in path.read_bytes() for path in rejected.iterdir())
        observer_path.write_bytes(observer_bytes); provenance_path.write_bytes(provenance_bytes)
        for mode in ("missing","changed","unapproved_manifest","exit_mismatch"):
            provenance = json.loads(provenance_bytes)
            if mode=="missing": observer_path.unlink()
            elif mode=="changed": observer_path.write_bytes(observer_bytes+b" ")
            elif mode=="unapproved_manifest": provenance["restartObserver"]["private"]="UNAPPROVED_PRIVATE_SENTINEL"
            else: provenance["restartObserver"]["observerExitCode"]=1
            provenance_path.write_text(json.dumps(provenance))
            rejected,passed,details = export(base)
            assert not passed and details["genuineReceipts"]==5 and details["restartObserverStatus"]=="UNVERIFIED"
            assert all(b"UNAPPROVED_PRIVATE_SENTINEL" not in path.read_bytes() for path in rejected.iterdir())
            observer_path.write_bytes(observer_bytes); provenance_path.write_bytes(provenance_bytes)
        _,passed,_ = export(base)
        assert passed, "observer rejection tests need a restored positive control"
        output, passed, summary = export(base,1)
        assert not passed and summary["genuineReceipts"] == 5
        (public / facts[-1]["public"]).unlink()
        partial = {**deepcopy(base),"status":"FAIL","genuine_receipts":deepcopy(facts[:4])}
        output, passed, summary = export(partial,1)
        assert not passed and summary["genuineReceipts"] == 4 and not (output / facts[-1]["receipt"]).exists()
        sentinel = "UNAPPROVED_PRIVATE_SENTINEL"
        for name in (".progress.json.unregistered-temp", "private-runtime.json"):
            (public / name).write_text(sentinel)
        partial["raw_exception"] = sentinel
        output, passed, summary = export(partial,1)
        assert not passed and summary["genuineReceipts"] == 4
        assert all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())
        assert not (output / "private-runtime.json").exists() and not any(path.name.startswith('.') for path in output.iterdir())
        invalid = deepcopy(partial); invalid["genuine_receipts"][1]["official_host_verified"] = False
        output, passed, summary = export(invalid)
        assert not passed and summary["genuineReceipts"] == 3 and not (output / facts[1]["receipt"]).exists()
        invalid = deepcopy(partial); rejected = deepcopy(facts[0]); rejected["official_host_verified"] = False
        invalid["genuine_receipts"].insert(0,rejected)
        output, passed, summary = export(invalid)
        assert not passed and summary["genuineReceipts"] == 4 and (output / facts[0]["receipt"]).exists()
        receipt = public / facts[0]["receipt"]; original_receipt = receipt.read_bytes(); receipt.write_bytes(b"changed synthetic receipt")
        output, passed, summary = export(partial)
        assert not passed and summary["genuineReceipts"] == 3 and not (output / facts[0]["receipt"]).exists()
        receipt.write_bytes(original_receipt)
        metadata = public / facts[0]["public"]; original_metadata = metadata.read_bytes(); metadata.write_text(json.dumps({"journalHash":"a"*64,"private":sentinel}))
        output, passed, summary = export(partial)
        assert not passed and summary["genuineReceipts"] == 3 and all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())
        metadata.write_bytes(original_metadata)
        invalid = deepcopy(partial); invalid["genuine_receipts"][0]["public"] = sentinel
        output, passed, summary = export(invalid)
        assert not passed and summary["genuineReceipts"] == 3 and all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())
        invalid = deepcopy(partial); invalid["status"] = sentinel
        output, passed, summary = export(invalid)
        assert not passed and summary["nativeStatus"] == "UNVERIFIED" and summary["genuineReceipts"] == 4
        assert all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())
        terminal.write_text(sentinel)
        output, passed, summary = export(partial)
        assert not passed and summary["genuineReceipts"] == 4 and not (output / "native-result.json").exists()
        (public / "progress.json").write_text(sentinel)
        output, passed = protected_public(run,expected,1)
        assert not passed and json.loads((output / "summary.json").read_text())["genuineReceipts"] == 0
        assert not (output / "progress.json").exists()
        terminal.write_text(json.dumps({"marker":"synthetic exporter selfcheck only"}))
        (public / "progress.json").write_text(json.dumps(partial))
        package = run / "package"; package.mkdir(); (package / "BUILD-INFO.json").write_text("{}")
        exporter = protected_public
        for mode in ("partial","exception"):
            github_output = run / ("github-output-"+mode)
            process = Mock(wait=Mock(return_value=1),poll=Mock(return_value=1))
            def publish(*_args):
                if mode == "exception": raise RuntimeError(sentinel)
                return exporter(run,expected,1)
            with patch(f"{__name__}.source_commit"), patch(f"{__name__}.checked_package"), patch(f"{__name__}.command"), \
                 patch(f"{__name__}.protected_public",side_effect=publish), \
                 patch("runpy.run_path",return_value={"sandbox_commands":lambda _context:(["synthetic","build"],original)}), \
                 patch("subprocess.check_output",side_effect=[str(common)+"\n",str(common/"worktrees/task")+"\n"]), \
                 patch("subprocess.Popen",return_value=process), \
                 patch("subprocess.run",return_value=subprocess.CompletedProcess([],1,b"",b"No such container")), \
                 patch.dict(os.environ,{"GITHUB_OUTPUT":str(github_output)}), patch("builtins.print"):
                try: protected_package_dispatch(package,expected,run/("host-"+mode))
                except RuntimeError as error: assert sentinel not in str(error)
                else: raise AssertionError("failed native/export runs cannot pass")
            output = Path(github_output.read_text().strip().split("=",1)[1])
            summary = json.loads((output/"summary.json").read_text())
            assert summary["status"] == "FAIL" and summary["genuineReceipts"] == (4 if mode == "partial" else 0)
            assert all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())


    self_test_registration_export(expected)
    self_test_proof_export(expected)


def self_test_registration_export(expected):
    from copy import deepcopy
    from unittest.mock import patch
    driver = runpy.run_path(str(ROOT / "scripts/verify-delivery-protected.py"))
    artifacts = {"prover":"b"*64,"r0vm":"c"*64,"profile":"d"*64}
    records = driver["self_test_diagnostic_fixture"]()
    data = b"".join((json.dumps(record,separators=(",",":"))+"\n").encode() for record in records)
    diagnostic = driver["registration_diagnostics"](data,expected,artifacts,{"exit_code":0,"signal":None,"spawn_errno":None})
    assert diagnostic["availability"]=="complete" and len(diagnostic["resource_deltas"])==8
    def forbidden(*_args,**_kwargs): raise AssertionError("diagnostic export self-test started business work")
    with tempfile.TemporaryDirectory(prefix="registration-diagnostic-export-selftest-",dir=ARTIFACTS) as temporary, \
         patch("subprocess.Popen",forbidden),patch("subprocess.run",forbidden),patch("socket.socket",forbidden):
        run=Path(temporary)
        self_test_observer_fixture(run,expected,{"source_commit":expected,"artifact_sha256":artifacts})
        public=run/"native/public"
        observer,passed=write_observer_summary(run,expected,0,0)
        assert passed
        write_private(run/"artifact-provenance.json",{"sourceCommit":expected,"status":"PASS","testedArtifactSha256":artifacts,"restartObserver":observer})
        write_private(run/"resources.json",{"status":"PASS"})
        facts=[]
        for relation,order in [(1,None),(2,"ab"*32),(3,"ab"*32),(2,"cd"*32),(3,"cd"*32)]:
            name="key-binding" if relation==1 else f"order-{order}-relation-{relation}-window-0"
            receipt=public/(name+".receipt");receipt.write_bytes(b"synthetic diagnostic-export control only: "+name.encode())
            metadata=public/(name+".json");metadata.write_text(json.dumps({"journalHash":"a"*64}))
            fact={"relation":relation,"public":metadata.name,"receipt":receipt.name,"receiptSha256":digest(receipt),"publicSha256":digest(metadata),
                  "journalHash":"a"*64,"official_host_verified":True,"wrong_journal_receipt_comparison_rejected":True}
            if order: fact.update(orderId="0x"+order,windowIndex=0)
            facts.append(fact)
        def export(value,code=0):
            diagnostic_path=public/"registration-diagnostics.json"
            diagnostic_path.write_text(json.dumps(value))
            manifest={"source_commit":expected,"scope":"fresh-two-order-protected-native-only","status":"PASS","genuine_receipts":facts,
                "public_files":{"native-result.json":digest(public/"native-result.json"),"registration-diagnostics.json":digest(diagnostic_path)}}
            (public/"progress.json").write_text(json.dumps(manifest))
            return protected_public(run,expected,code)
        output,passed=export(diagnostic)
        assert passed
        projected=json.loads((output/"registration-diagnostics.json").read_bytes())
        assert projected==diagnostic and len(projected["events"])==len(records) and len(projected["resource_deltas"])==8
        boundary=next(index for index,event in enumerate(records) if event["phase"]=="worker_wait")
        failed=deepcopy(records[:boundary+1])
        failed[-1].update(edge="error",exit_code=137,failure_class="worker_exit")
        partial=driver["registration_diagnostics"](b"".join((json.dumps(record,separators=(",",":"))+"\n").encode() for record in failed),
            expected,artifacts,{"exit_code":1,"signal":None,"spawn_errno":None})
        assert partial["availability"]=="partial" and partial["first_observed_failure"]["exit_code"]==137
        output,passed=export(partial,1)
        assert not passed and json.loads((output/"registration-diagnostics.json").read_bytes())==partial
        invalid_exit=deepcopy(partial)
        invalid_exit["events"][-1]["exit_code"]=0
        invalid_exit["first_observed_failure"]["exit_code"]=0
        missing_wait_status=deepcopy(diagnostic)
        for event in missing_wait_status["events"]:
            if event["phase"]=="worker_wait": event["exit_code"]=None
        for invalid in (invalid_exit,missing_wait_status):
            output,passed=export(invalid)
            assert not passed and not (output/"registration-diagnostics.json").exists()
        sentinel="UNAPPROVED_PRIVATE_SENTINEL"
        changes=[lambda value:value.update(private=sentinel),lambda value:value.update(source_commit="0"*40),
            lambda value:value["artifact_sha256"].update(prover="0"*64),lambda value:value["events"][0].update(private=sentinel),
            lambda value:value["events"][0].update(exit_code=True),lambda value:value["resource_deltas"].append(deepcopy(value["resource_deltas"][0]))]
        for change in changes:
            invalid=deepcopy(diagnostic);change(invalid)
            output,passed=export(invalid)
            assert not passed and not (output/"registration-diagnostics.json").exists()
            assert all(sentinel.encode() not in item.read_bytes() for item in output.iterdir())
        output,passed=export(diagnostic,1)
        assert not passed and (output/"registration-diagnostics.json").exists()
        unavailable=driver["registration_diagnostics"](b"",expected,artifacts,{"exit_code":None,"signal":9,"spawn_errno":None})
        output,passed=export(unavailable,1)
        assert not passed and json.loads((output/"registration-diagnostics.json").read_bytes())["availability"]=="unavailable"
    print(f"Registration diagnostic export self-tests passed; full synthetic {len(records)}-event path, 8 resource groups, strict privacy negatives; no processes or proofs")


def self_test_proof_export(expected):
    from copy import deepcopy
    from unittest.mock import patch
    driver = runpy.run_path(str(ROOT / "scripts/verify-delivery-protected.py"))
    profile_bytes = regular(ROOT / "crates/delivery/proof/profile.json", 16384)
    profile = driver["profile_id"](driver["diagnostic_json"](profile_bytes))
    profile_value = json.loads(profile_bytes)
    artifacts = {name: "a" * 64 for name in ("binary", "core_binary", "crypto_module", "crypto_wasm", "prover", "r0vm", "profile")}
    artifacts.update(prover="b" * 64, r0vm="c" * 64, profile=hashlib.sha256(profile_bytes).hexdigest())
    guest = {"imageId": bytes(profile_value["guest_image_id"]).hex(), "schemaHash": bytes(profile_value["statement_schema_hash"]).hex(),
             "elfSha256": "e" * 64, "statementBytes": 1504}
    principals = {node: {"peer": value * 32} for node, value in (("S", "11"), ("R", "22"))}
    orders = [{"order_id": value * 32, "sha256": "f" * 64} for value in ("ab", "cd")]
    fixtures = {(node, index): driver["self_test_proof_fixture"](node, index, artifacts, profile) for node in ("S", "R") for index in range(2)}
    facts = [{"relation": 1, "public": "key-binding.json", "receipt": "key-binding.receipt", "journalHash": "a" * 64,
              "official_host_verified": True, "wrong_journal_receipt_comparison_rejected": True}]
    metadata = {"key-binding.json": {"journalHash": "a" * 64, "profileId": profile, "imageId": guest["imageId"]}}
    receipts = {"key-binding.receipt": b"synthetic key receipt for proof diagnostic exporter selfcheck"}
    for (node, index), records in fixtures.items():
        relation, order = records[0]["relation"], orders[index]["order_id"]
        name = f"order-{order}-relation-{relation}-window-0"
        metadata[name + ".json"] = {"journalHash": records[0]["job"], "profileId": profile, "imageId": guest["imageId"]}
        receipts[name + ".receipt"] = b"synthetic order receipt for exporter selfcheck: " + name.encode()
        facts.append({"relation": relation, "orderId": "0x" + order, "windowIndex": 0, "public": name + ".json", "receipt": name + ".receipt",
                      "journalHash": records[0]["job"], "official_host_verified": True, "wrong_journal_receipt_comparison_rejected": True})
    for fact in facts:
        fact["receiptSha256"] = hashlib.sha256(receipts[fact["receipt"]]).hexdigest()
        fact["publicSha256"] = hashlib.sha256(json.dumps(metadata[fact["public"]]).encode()).hexdigest()
    jobs = [driver["summarize_proof_job"](b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in records),
        node, index, principals[node]["peer"], orders[index]["order_id"], profile, artifacts, [1, 2], records[0]["job"], facts)
        for (node, index), records in fixtures.items()]
    diagnostic = {"schema_version": 1, "source_commit": expected, "artifact_sha256": {name: artifacts[name] for name in ("prover", "r0vm", "profile")},
                  "profile_id": profile, "counter_fields": list(driver["DIAGNOSTIC_COUNTERS"]), "attempt_policy": "first_admitted", "availability": "complete", "jobs": jobs}
    native = {"source_commit": expected, "artifact_sha256": artifacts, "profile_id": profile, "prover_info": guest,
              "principals": principals, "orders": orders, "genuine_receipts": facts, "proof_diagnostics": diagnostic}
    def forbidden(*_args, **_kwargs): raise AssertionError("proof diagnostic export selfcheck started business work")
    negative_count = 0
    with tempfile.TemporaryDirectory(prefix="proof-diagnostic-export-selftest-", dir=ARTIFACTS) as temporary, \
         patch("subprocess.Popen", forbidden), patch("subprocess.run", forbidden), patch("socket.socket", forbidden):
        parent = Path(temporary)
        def export(value, *, code=0, wrapper_artifacts=None, duplicate=False):
            run = parent / uuid.uuid4().hex; run.mkdir(mode=0o700)
            self_test_observer_fixture(run, expected, value)
            public = run / "native/public"
            for name, data in receipts.items(): (public / name).write_bytes(data)
            for name, data in metadata.items(): (public / name).write_text(json.dumps(data))
            if duplicate:
                path = public / "native-result.json"
                path.write_bytes(path.read_bytes().replace(b'"proof_diagnostics":', b'"proof_diagnostics":null,"proof_diagnostics":', 1))
                # Authenticate the altered original bytes too; the parser, not a stale hash, must reject them.
                bindings = observer_bindings(run, expected)
                raw = json.loads((run / "restart-observer/result.json").read_bytes())
                raw["native_result_sha256"] = bindings["nativeResultSha256"]
                (run / "restart-observer/result.json").write_text(json.dumps(raw))
            observer, passed = write_observer_summary(run, expected, 0, 0)
            assert passed
            write_private(run / "artifact-provenance.json", {"sourceCommit": expected, "status": "PASS", "testedArtifactSha256": wrapper_artifacts or artifacts,
                "guestInfo": guest, "restartObserver": observer})
            write_private(run / "resources.json", {"status": "PASS"})
            raw_bytes = (public / "native-result.json").read_bytes()
            write_private(public / "progress.json", {"source_commit": expected, "scope": "fresh-two-order-protected-native-only", "status": "PASS",
                "genuine_receipts": facts, "public_files": {"native-result.json": hashlib.sha256(raw_bytes).hexdigest()}})
            output, passed = protected_public(run, expected, code)
            return output, passed, raw_bytes
        output, passed, raw_bytes = export(native)
        assert passed and (output / "native-result.json").read_bytes() == raw_bytes
        assert json.loads(raw_bytes)["proof_diagnostics"]["availability"] == "complete" and len(jobs) == 4
        missing = deepcopy(native); missing.pop("proof_diagnostics")
        output, passed, raw_bytes = export(missing)
        assert passed and (output / "native-result.json").read_bytes() == raw_bytes
        unavailable = deepcopy(native)
        unavailable["proof_diagnostics"].update(availability="unavailable", jobs=[driver["unavailable_proof_job"](job["node"], job["order_index"]) for job in jobs])
        for code in (0, 1):
            output, passed, raw_bytes = export(unavailable, code=code)
            assert passed == (code == 0) and (output / "native-result.json").read_bytes() == raw_bytes
        changes = [lambda value: value["proof_diagnostics"].update(source_commit="0" * 40),
                   lambda value: value["proof_diagnostics"]["artifact_sha256"].update(prover="0" * 64),
                   lambda value: value["proof_diagnostics"].update(profile_id="0" * 64),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(node="R"),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(order_index=1),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(relation=3),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(window=1),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(job_digest="0" * 64),
                   lambda value: value["proof_diagnostics"].update(private="PRIVATE_ORDER_DIAGNOSTIC_SENTINEL"),
                   lambda value: value["proof_diagnostics"]["jobs"][0]["source_activity"]["start"].update(private="PRIVATE_ORDER_DIAGNOSTIC_SENTINEL"),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(mode="receipt_reconcile"),
                   lambda value: value["artifact_sha256"].update(core_binary="0" * 64),
                   lambda value: value["prover_info"].update(elfSha256="0" * 64)]
        for change in changes:
            invalid = deepcopy(native); change(invalid)
            output, passed, _ = export(invalid)
            assert not passed and not (output / "native-result.json").exists()
            assert json.loads((output / "summary.json").read_bytes())["genuineReceipts"] == 5
            assert all(b"PRIVATE_ORDER_DIAGNOSTIC_SENTINEL" not in path.read_bytes() for path in output.iterdir())
            negative_count += 1
        output, passed, _ = export(native, wrapper_artifacts={**artifacts, "r0vm": "0" * 64})
        assert not passed and not (output / "native-result.json").exists(); negative_count += 1
        output, passed, _ = export(native, duplicate=True)
        assert not passed and not (output / "native-result.json").exists(); negative_count += 1
    print(json.dumps({"proof_diagnostic_export_selftest": "PASS", "concrete_jobs": 4, "qualified_pairs": 5,
                      "negative_controls": negative_count, "native_bytes_preserved": True, "external_processes": 0, "network_sockets": 0}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--verify-source-commit", metavar="SHA", help="Verify the exact clean checkout without starting a build or service")
    parser.add_argument("--source-commit")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--protected-config", type=Path, help="owner-only final config for fresh protected native acceptance inside the bounded Linux proof sandbox")
    parser.add_argument("--protected-run-dir", type=Path, help="fresh evidence directory inside this worktree's .artifacts")
    parser.add_argument("--protected-selfcheck", action="store_true", help="check the current protected driver without starting processes or proofs")
    parser.add_argument("--protected-package", type=Path, help="run protected native acceptance from an exact-head Linux package in the canonical bounded sandbox")
    parser.add_argument("--protected-inside", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.protected_package:
        if not args.source_commit or any((args.self_test, args.verify_source_commit, args.protected_config, args.protected_selfcheck)) or (args.protected_inside and not args.protected_run_dir):
            parser.error("Provide --protected-package and --source-commit, with an optional fresh --protected-run-dir")
        if args.protected_inside:
            protected_package_inside(args.protected_package, args.source_commit, args.protected_run_dir)
        else:
            protected_package_dispatch(args.protected_package, args.source_commit, args.protected_run_dir)
    elif args.protected_inside:
        parser.error("--protected-inside requires the canonical protected package launcher")
    elif args.protected_config or args.protected_selfcheck or args.protected_run_dir:
        if any((args.source_commit, args.self_test, args.verify_source_commit)) or bool(args.protected_config) == args.protected_selfcheck or (args.protected_run_dir and not args.protected_config):
            parser.error("Provide --protected-config with optional --protected-run-dir, or --protected-selfcheck")
        if args.protected_selfcheck:
            ARTIFACTS.mkdir(parents=True,exist_ok=True)
            protected_selfcheck()
        else:
            protected_dispatch(args.protected_config, args.protected_run_dir)
    elif args.verify_source_commit is not None:
        if args.source_commit or args.self_test:
            parser.error("--verify-source-commit cannot be combined with another action")
        source_commit(args.verify_source_commit)
    elif args.self_test and not args.source_commit:
        ARTIFACTS.mkdir(parents=True, exist_ok=True)
        self_test()
    else:
        parser.error("Provide --protected-package and --source-commit, --protected-config, --protected-selfcheck, --self-test, or --verify-source-commit")
