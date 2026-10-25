#!/usr/bin/env python3
"""Generate and observe one real local paid delivery in the bounded Linux sandbox."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import shutil
import subprocess
import tarfile
import tempfile
import time
import uuid


ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / ".artifacts/vdp"
PUBLIC = ARTIFACTS / "live-public"
PROOFS = ["key-binding", *[f"relation-{relation}-window-{index}" for index in range(2) for relation in (2, 3, 4)]]


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
    if actual != expected or subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=ROOT):
        raise ValueError("Live delivery requires the clean exact checkout used by the artifact job")


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


def live_container(command, package, expected):
    result = command[:-4]
    result.extend(["--env", "TOKIO_WORKER_THREADS=2", command[-4], "python3", "scripts/run-delivery-live.py", "--inside",
                   "--package", str(Path("/work") / package.relative_to(ROOT)), "--source-commit", expected])
    return result


def dispatch(package_input, expected):
    source_commit(expected)
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
    result = subprocess.run(live_container(container, package, expected), cwd=ROOT)
    if result.returncode:
        raise SystemExit(result.returncode)


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


def public_exports(run):
    bundle = run / "settlement/settlement.json"
    value = json.loads(regular(bundle))
    if set(value) != {"schemaVersion", "escrow", "asset", "orderId", "proofs", "capsules", "transactions"}:
        raise ValueError("Settlement export contains unexpected private fields")
    expected = {(name + ".receipt", name + ".json") for name in PROOFS}
    if {(proof["receipt"], proof["public"]) for proof in value["proofs"]} != expected or len(value["proofs"]) != 7:
        raise ValueError("Live settlement must export exactly seven identified proofs")
    for name in PROOFS:
        proof = json.loads(regular(run / "settlement" / (name + ".json")))
        if set(proof) != {"mode", "kind", "relation", "imageId", "profileId", "journal", "journalHash", "seal"} or proof["kind"] != "groth16":
            raise ValueError("Only public genuine proof metadata may be exported")
        for extension in (".json", ".receipt"):
            source = run / "settlement" / (name + extension)
            regular(source, 16384)
            shutil.copyfile(source, PUBLIC / (name + extension))
    shutil.copyfile(bundle, PUBLIC / "settlement.json")
    observation = ROOT / ".artifacts/vdp/contracts/settlement-observation.json"
    observed = json.loads(regular(observation))
    if observed.get("status") != "PASS" or any(observed.get("money", {}).get(name) != "29" for name in ("funded", "earned", "withdrawn")):
        raise ValueError("Read-only observer did not confirm the exact actual payment")
    shutil.copyfile(observation, PUBLIC / "settlement-observation.json")
    return value


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
    PUBLIC.mkdir(parents=True, mode=0o755, exist_ok=False)
    log = run / "private-task.log"
    cgroup, initial = resources()
    summary = {"schemaVersion": 1, "status": "FAIL", "sourceCommit": expected, "mode": "local TestToken on loopback chain 31337",
               "notChecked": ["real assets or public deployment", "absolute chain finality", "rendered Web or native mobile UI"]}
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
        command("seven genuine proofs, exact withdrawal and read-only observation", ["python3", "scripts/verify-node.py", "--delivery", "--binary", str(binary),
                "--crypto-module", str(module), "--artifacts", str(run / "process"), "--delivery-probe", str(probe_target / "debug/vibeface-delivery-probe"),
                "--delivery-config", str(configuration), "--delivery-source-root", "/work", "--delivery-profile", "/work/proofs/delivery/profile.json"], log, env=environment)
        results = list((run / "process").glob("delivery-paid-*/summary.json"))
        if len(results) != 1:
            raise ValueError("Expected exactly one actual paid process acceptance result")
        original_file = checked_capacity_case(json.loads(regular(results[0])))
        public_exports(results[0].parent)
        summary.update(status="PASS", genuineProofs=7, earned="29", withdrawn="29", readOnlyObserver=True, originalFile=original_file)
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
        for file in PUBLIC.iterdir():
            file.chmod(0o644)
        if resource_failure:
            raise RuntimeError("Live delivery resource acceptance failed: " + resource_failure)
    print("PASS delivery-live seven genuine proofs and exact local payment", flush=True)


def self_test():
    from copy import deepcopy
    from unittest.mock import patch
    checks = runpy.run_path(str(ROOT / "scripts/check-delivery.py"))
    _, original = checks["sandbox_commands"](Path("/context"))
    package = ROOT / ".artifacts/vdp/live-input/package"
    command_line = live_container(original, package, "1" * 40)
    assert command_line.count("--mount") == 4 and "--privileged" not in command_line
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
    print("Live dispatch, capacity evidence, OOM counters, protected configuration and GLIBC classification self-tests passed; no runtime or proof ran")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path)
    parser.add_argument("--source-commit")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        ARTIFACTS.mkdir(parents=True, exist_ok=True)
        self_test()
    elif args.package and args.source_commit:
        inside(args.package, args.source_commit) if args.inside else dispatch(args.package, args.source_commit)
    else:
        parser.error("Provide --package and --source-commit")
