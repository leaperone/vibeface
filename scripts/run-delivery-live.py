#!/usr/bin/env python3
"""Run protected native acceptance and validate its public evidence."""

import sys
sys.dont_write_bytecode = True
import release_context
import release_format

import argparse
import json
import os
from pathlib import Path
import re
import runpy
import signal
import shutil
import subprocess
import tarfile
import tempfile
import time
import uuid

from verification.resource import oom_delta, resources
from verification.io import digest_bytes, digest_file as digest, regular, replace_public, write_private
from verification.process import stop_owned_process


ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / ".artifacts/vdp"


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
    release_format.verify_build_info(info, release_context.contracts(), release_context.runtime_definitions())
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
    """Stop the private delivery child process group and preserve escalation timing."""
    if process is None or process.poll() is not None:
        return
    stop_owned_process(
        process,
        terminate=lambda: os.killpg(process.pid, signal.SIGTERM),
        kill=lambda: os.killpg(process.pid, signal.SIGKILL),
        wait_timeout=10,
        kill_wait_timeout=5,
        return_on_terminate_error=True,
    )


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
        "configurationSha256":digest_bytes(configuration),"scriptSha256":digest(ROOT / OBSERVER_SCRIPT),
        "proverSha256":config["sha256"]["prover"],"nativeResultSha256":digest_bytes(regular(native,131072)) if native.is_file() and not native.is_symlink() else None}


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
            release_format.verify_build_info(package["buildInfo"], release_context.contracts(), release_context.runtime_definitions())
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
            if digest_bytes(receipt_bytes) != fact["receiptSha256"] or digest_bytes(metadata_bytes) != fact["publicSha256"] or metadata["journalHash"] != fact["journalHash"]:
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
            if not isinstance(checksum,str) or not re.fullmatch(r"[0-9a-f]{64}",checksum) or digest_bytes(data) != checksum:
                raise ValueError("changed public report")
            if name == "registration-diagnostics.json":
                driver = runpy.run_path(str(ROOT / "scripts/verify-delivery-protected.py"))
                native_bytes = regular(source / "native-result.json",131072)
                if digest_bytes(native_bytes) != certified_files.get("native-result.json"):
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
                        if digest_bytes(profile_bytes) != projected["artifact_sha256"]["profile"] \
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
        if digest_bytes(data) != manifest["sha256"]:
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
            from tests.delivery.test_live_runtime import run_protected_selfcheck

            run_protected_selfcheck(sys.modules[__name__])
        else:
            protected_dispatch(args.protected_config, args.protected_run_dir)
    elif args.verify_source_commit is not None:
        if args.source_commit or args.self_test:
            parser.error("--verify-source-commit cannot be combined with another action")
        source_commit(args.verify_source_commit)
    elif args.self_test and not args.source_commit:
        ARTIFACTS.mkdir(parents=True, exist_ok=True)
        from tests.delivery.test_live_runtime import run_live_tests

        run_live_tests(sys.modules[__name__])
    else:
        parser.error("Provide --protected-package and --source-commit, --protected-config, --protected-selfcheck, --self-test, or --verify-source-commit")
