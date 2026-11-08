"""Project one task-owned XCTest prepare failure without publishing raw SDK logs."""

from __future__ import annotations

import json
import math
import os
from pathlib import Path
import re
import stat
import sys


SESSION = "vdp-native-acceptance"
LIMIT = 262144
HEALTH_REASONS = {"runner_not_connected", "endpoint_probe_failed", "unknown"}
XCODE_FAILURES = {
    "empty_supported_platforms": r"Supported platforms for the buildables in the current scheme is empty",
    "runtime_not_installed": r"(?:iOS|runtime)[^\n]{0,120}(?:not installed|not available)",
    "destination_not_found": r"Unable to find a destination matching|no available devices matched",
    "service_hub_unavailable": r"Failed to bless service hub|DTServiceHubClient",
    "sdk_not_found": r"SDK[^\n]{0,120}(?:cannot be located|not found)|unable to find sdk",
    "architecture_mismatch": r"incompatible architecture|does not contain[^\n]{0,80}architecture",
    "signing_configuration": r"requires a development team|requires a provisioning profile",
    "test_host_missing": r"test host[^\n]{0,120}does not exist|failed to find[^\n]{0,80}test bundle",
}


def number(value):
    return type(value) in {int, float} and 0 <= value <= 600000 and math.isfinite(value)


STARTUP_PHASES = {"adopt_detached_runner", "ensure_booted", "verify_device_readiness", "verify_host_dev_tools_security", "cleanup_stale_bundles", "ensure_xctestrun", "build_xctestrun", "allocate_port", "prepare_xctestrun_env", "launch_xcodebuild"}
LAUNCH_MARKERS = {"HEADLESS_STARTUP", "LISTENER_CONFIGURATION_MISSING", "LISTENER_READY", "PORT_NOT_SET", "LISTENER_FAILED", "WAITING"}


def project_startup(value):
    if not isinstance(value, dict) or value.get("status") not in {"captured", "ambiguous"}:
        return {"status": "unavailable"}
    result = {"status": value["status"]}
    if value["status"] == "captured":
        if type(value.get("ready")) is bool:
            result["ready"] = value["ready"]
        if number(value.get("duration_ms")):
            result["duration_ms"] = value["duration_ms"]
        timings = value.get("timings")
        if isinstance(timings, dict):
            result["timings"] = {key: item for key, item in timings.items() if key in STARTUP_PHASES and number(item)}
    return result


def project_health(value):
    if not isinstance(value, dict) or value.get("status") not in {"captured", "not_recorded", "unavailable", "ambiguous"}:
        return {"status": "unavailable"}
    result = {"status": value["status"]}
    if "startup" in value:
        result["startup"] = project_startup(value["startup"])
    if value["status"] == "captured":
        result["reason"] = value.get("reason") if value.get("reason") in HEALTH_REASONS else "unknown"
        for key in ("duration_ms", "timeout_ms"):
            if number(value.get(key)):
                result[key] = value[key]
        if type(value.get("session_ready")) is bool:
            result["session_ready"] = value["session_ready"]
    if type(value.get("cache_invalidated")) is bool:
        result["cache_invalidated"] = value["cache_invalidated"]
    return result


def project_error(error):
    codes = {"INVALID_ARGS", "DEVICE_NOT_FOUND", "DEVICE_IN_USE", "TOOL_MISSING", "APP_NOT_INSTALLED",
             "UNSUPPORTED_PLATFORM", "UNSUPPORTED_OPERATION", "NOT_IMPLEMENTED", "COMMAND_FAILED",
             "SESSION_NOT_FOUND", "UNAUTHORIZED", "AMBIGUOUS_MATCH", "REPLAY_DIVERGENCE",
             "REPAIR_SESSION_EXPIRED", "REPAIR_COMMIT_FAILED", "UNKNOWN"}
    if not isinstance(error, dict):
        return {"code": "unknown"}
    code = error.get("code")
    result = {"code": code if isinstance(code, str) and code in codes else "unknown"}
    if "initial_health" in error:
        result["initial_health"] = project_health(error["initial_health"])
    details = error.get("details")
    if not isinstance(details, dict):
        return result
    projected = {}
    for key, allowed in (
        ("reason", {"runner_start_budget_exhausted", "runner_phase_budget_exhausted", "prepare_deadline_expired",
                    "simulator_set_destination_not_found", "simulator_service_hub_unavailable", "build_failed_unclassified",
                    "bundle_identifier_already_registered", "signing_no_development_team", "signing_provisioning_profile_missing",
                    "signing_unspecified", "devtools_security_developer_mode_disabled", "device_developer_disk_image_unavailable",
                    "IOS_RUNNER_CONNECT_TIMEOUT", "IOS_RUNNER_DEVICE_NOT_PROVISIONED", "IOS_BOOT_TIMEOUT", "IOS_TOOL_MISSING",
                    "BOOT_COMMAND_FAILED", "CI_RESOURCE_STARVATION_SUSPECTED", "UNKNOWN", "request_canceled",
                    "daemon_transport_timeout", "daemon_transport_failure", "startup-stalled", "startup-ceiling", "startup-exited"}),
        ("runnerConnectFailureReason", {"runner_connect_refused", "runner_endpoint_probe_exhausted", "xcodebuild_exited_early"}),
        ("phase", {"runner_session", "runner_rebuild", "runner_health", "runner_xctestrun_build"}),
    ):
        value = details.get(key)
        if isinstance(value, str) and value in allowed:
            projected[key] = value
    for key in ("timeoutMs", "startupTimeoutMs"):
        value = details.get(key)
        if type(value) in {int, float} and 0 <= value <= 600000:
            projected[key] = value
    attempts = details.get("startupAttempts")
    if type(attempts) is int and 1 <= attempts <= 2:
        projected["startupAttempts"] = attempts
    xcodebuild_result = details.get("xcodebuild")
    value = xcodebuild_result.get("exitCode") if isinstance(xcodebuild_result, dict) else None
    if type(value) is int and 0 <= value <= 255:
        kind = xcodebuild_result.get("failure_kind")
        kind = kind if isinstance(kind, str) and kind in XCODE_FAILURES else "unknown"
        stderr = xcodebuild_result.get("stderr")
        if isinstance(stderr, str) and len(stderr.encode("utf-8", "replace")) <= 32768:
            kind = next((key for key, pattern in XCODE_FAILURES.items() if re.search(pattern, stderr, re.IGNORECASE)), "unknown")
        projected["xcodebuild"] = {"exitCode": value, "failure_kind": kind}
    if projected:
        result["details"] = projected
    return result



def bounded_file(root, path, *, tail=False):
    relative = path.absolute().relative_to(root.absolute())
    descriptors = [os.open(root.resolve(), os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)]
    try:
        if not relative.parts or any(part in {"", ".", ".."} for part in relative.parts):
            raise ValueError("invalid task file")
        for part in relative.parts[:-1]:
            descriptors.append(os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptors[-1]))
        descriptors.append(os.open(relative.parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=descriptors[-1]))
        info = os.fstat(descriptors[-1])
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or not tail and info.st_size > LIMIT:
            raise ValueError("invalid task file")
        if tail:
            os.lseek(descriptors[-1], max(0, info.st_size - 65536), os.SEEK_SET)
        data = os.read(descriptors[-1], 65536 if tail else LIMIT + 1)
        if not tail and len(data) > LIMIT:
            raise ValueError("task file too large")
        return data
    finally:
        for descriptor in reversed(descriptors):
            os.close(descriptor)


def health_records(raw, session, request_id):
    first = None
    invalidated = False
    sends = 0
    invalidated_before_send = False
    startup = []
    for line in raw.decode("utf-8").splitlines():
        if not line:
            continue
        row = json.loads(line)
        if not isinstance(row, dict) or row.get("session") != session or row.get("requestId") != request_id or row.get("command") != "prepare":
            raise ValueError("request record identity changed")
        data = row.get("data")
        if not isinstance(data, dict):
            continue
        if row.get("phase") == "ios_runner_session_startup_timings":
            startup.append(project_startup({"status": "captured", "ready": data.get("ready"), "duration_ms": row.get("durationMs"), "timings": data.get("timings")}))
        if row.get("phase") == "ios_runner_session_invalidated" and data.get("reason") == "prepare_cached_runner_health_failed":
            invalidated = True
            invalidated_before_send = invalidated_before_send or first is None
        if row.get("phase") == "ios_runner_command_send" and row.get("level") == "error" and data.get("command") == "uptime" and data.get("readOnly") is True:
            sends += 1
            message = data.get("error")
            reason = {"Runner did not accept connection": "runner_not_connected", "Runner endpoint probe failed": "endpoint_probe_failed"}.get(message, "unknown") if isinstance(message, str) else "unknown"
            if first is None:
                first = project_health({"status": "captured", "reason": reason, "duration_ms": row.get("durationMs"), "timeout_ms": data.get("timeoutMs"), "session_ready": data.get("sessionReady")})
    if sends > 1 or invalidated_before_send:
        return {"status": "ambiguous"}
    result = {**(first or {"status": "not_recorded"}), "cache_invalidated": invalidated}
    if startup:
        result["startup"] = startup[0] if len(startup) == 1 else {"status": "ambiguous"}
    return result


def read_error(repo, raw_path):
    try:
        if not raw_path.is_absolute():
            raw_path = repo / raw_path
        value = json.loads(bounded_file(repo, raw_path))
        if not isinstance(value, dict) or value.get("success") is not False:
            return {"code": "unknown"}
        error = value.get("error")
        result = project_error(error)
        result["initial_health"] = {"status": "unavailable"}
        pointer = error.get("diagnosticsRecord") if isinstance(error, dict) else None
        if not isinstance(pointer, dict) or pointer.get("session") != SESSION:
            return result
        request_id = pointer.get("requestId")
        if not isinstance(request_id, str) or not re.fullmatch(r"[0-9a-f]{16}", request_id):
            return result
        path = repo / ".artifacts/native/agent-device/sessions" / SESSION / "requests" / (request_id + ".ndjson")
        result["initial_health"] = health_records(bounded_file(repo, path), SESSION, request_id)
        return result
    except (OSError, ValueError, TypeError, AssertionError, RecursionError):
        return locals().get("result", {"code": "unknown"})


def self_test():
    from tempfile import TemporaryDirectory
    request_id = "1234567890abcdef"
    marker = "private-message-id-path-token"
    row = dict(session=SESSION, requestId=request_id, command="prepare", level="error", phase="ios_runner_command_send", durationMs=451.5,
               data=dict(command="uptime", readOnly=True, timeoutMs=300, sessionReady=False, error="Runner did not accept connection", private=marker))
    invalidated = dict(session=SESSION, requestId=request_id, command="prepare", phase="ios_runner_session_invalidated", data=dict(reason="prepare_cached_runner_health_failed", deviceId=marker))
    encode = lambda rows: ("\n".join(json.dumps(item) for item in rows) + "\n").encode()
    expected = dict(status="captured", reason="runner_not_connected", duration_ms=451.5, timeout_ms=300, session_ready=False, cache_invalidated=True)
    assert health_records(encode([row, invalidated]), SESSION, request_id) == expected
    timing = dict(session=SESSION, requestId=request_id, command="prepare", phase="ios_runner_session_startup_timings", durationMs=800,
                  data=dict(ready=False, timings=dict(ensure_xctestrun=400, build_xctestrun=300, launch_xcodebuild=200, unknown=marker)))
    observed = health_records(encode([timing, row, invalidated]), SESSION, request_id)
    assert observed["startup"] == dict(status="captured", ready=False, duration_ms=800, timings=dict(ensure_xctestrun=400, build_xctestrun=300, launch_xcodebuild=200))
    assert project_health(observed) == observed and marker not in json.dumps(observed)
    assert health_records(encode([timing, timing, row]), SESSION, request_id)["startup"] == {"status": "ambiguous"}
    other = {**row, "data": {**row["data"], "error": marker}}
    assert health_records(encode([other]), SESSION, request_id)["reason"] == "unknown"
    for bad in ({**row, "requestId": "other"}, {**row, "session": "other"}, {**row, "command": "open"}):
        try:
            health_records(encode([bad]), SESSION, request_id)
        except ValueError:
            pass
        else:
            raise AssertionError("accepted another request")
    assert project_health({**expected, "duration_ms": float("inf"), "timeout_ms": True, "session_ready": marker}) == dict(status="captured", reason="runner_not_connected", cache_invalidated=True)
    assert project_health({**expected, "duration_ms": 10**1000}) == {key: value for key, value in expected.items() if key != "duration_ms"}
    for kind, message in (("destination_not_found", "Unable to find a destination matching the provided destination specifier"), ("runtime_not_installed", "iOS 26.5 is not installed"), ("unknown", marker)):
        projected = project_error({"code": "COMMAND_FAILED", "details": {"xcodebuild": {"exitCode": 70, "stderr": message}}})
        assert projected["details"]["xcodebuild"] == {"exitCode": 70, "failure_kind": kind}
        assert project_error(projected) == projected and marker not in json.dumps(projected)
    with TemporaryDirectory(prefix="ios-prepare-projection-") as temporary:
        root = Path(temporary)
        raw = root / ".artifacts/native/private/prepare.json"
        record = root / ".artifacts/native/agent-device/sessions" / SESSION / "requests" / (request_id + ".ndjson")
        raw.parent.mkdir(parents=True); record.parent.mkdir(parents=True)
        error = dict(code="COMMAND_FAILED", message=marker, details=dict(reason="prepare_deadline_expired", phase="runner_rebuild", raw=marker), diagnosticsRecord=dict(session=SESSION, requestId=request_id))
        raw.write_text(json.dumps(dict(success=False, error=error)))
        record.write_bytes(encode([row, invalidated]))
        large = root / "bounded-runner.log"
        large.write_bytes(b"x" * LIMIT + b"AGENT_DEVICE_RUNNER_LISTENER_READY")
        assert bounded_file(root, large, tail=True) == large.read_bytes()[-65536:]
        result = read_error(root, raw)
        assert result["initial_health"] == expected and result["details"]["reason"] == "prepare_deadline_expired"
        assert marker not in json.dumps(result) and request_id not in json.dumps(result)
        assert project_error(result) == result
        record.unlink(); record.symlink_to(raw)
        assert read_error(root, raw)["initial_health"] == {"status": "unavailable"}
        record.unlink(); record.write_bytes(b"x" * (LIMIT + 1))
        assert read_error(root, raw)["initial_health"] == {"status": "unavailable"}
        record.unlink(); record.parent.rename(record.parent.with_name("kept")); record.parent.symlink_to(record.parent.with_name("kept"))
        assert read_error(root, raw)["initial_health"] == {"status": "unavailable"}
    print("XCTest prepare error projection bounds passed")


def probe():
    import shutil
    import subprocess

    repo = Path(__file__).resolve().parents[1]
    run_id, attempt, source = (os.environ.get(key, "") for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA"))
    assert run_id.isdigit() and attempt.isdigit() and re.fullmatch(r"[a-f0-9]{40}", source)
    assert subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip() == source
    layout = __import__("runpy").run_path(str(repo / "scripts/ios-actor-census.py"))
    root = layout["inside_artifacts"](repo, repo / ".artifacts/native")
    os.umask(0o077)
    root.mkdir(parents=True, exist_ok=False)
    private = root / "private"
    private.mkdir(); (root / "public").mkdir()
    state = root / "agent-device"
    environment = {**os.environ, "AGENT_DEVICE_HOME": str(state), "AGENT_DEVICE_STATE_DIR": str(state), "AGENT_DEVICE_SESSION": SESSION}
    runtime = simulator = binary = None
    prepare_code = None
    stage = "device-selection"
    result = {"source_commit": source, "run_id": run_id, "run_attempt": attempt, "product_acceptance": False, "SDK_prepare_invocations": 0}
    cleanup = {"daemon_stopped": False, "simulator_removed": False, "private_state_removed": False}

    def command(arguments, label):
        with (private / (label + ".stdout")).open("xb") as output, (private / (label + ".stderr")).open("xb") as error:
            process = subprocess.run(arguments, cwd=repo, env=environment, stdout=output, stderr=error)
        if process.returncode:
            raise RuntimeError("probe command failed")
        return (private / (label + ".stdout")).read_bytes()

    try:
        resolver = 'const fs=require("node:fs"),path=require("node:path"); const entry=require.resolve("@e2e-dev/mobile"); const container=path.resolve(path.dirname(path.dirname(entry)),"..",".."); const pkg=fs.realpathSync(path.join(container,"agent-device")); const bin=path.join(pkg,"bin","agent-device.mjs"); if(!fs.existsSync(bin))process.exit(2); process.stdout.write(bin)'
        binary = command(["node", "-e", resolver], "resolve-sdk").decode().strip()
        runtimes = json.loads(command(["xcrun", "simctl", "list", "runtimes", "--json"], "runtimes"))["runtimes"]
        available = [row for row in runtimes if row.get("isAvailable") and ".SimRuntime.iOS-" in row["identifier"]]
        selected = max(available, key=lambda row: tuple(map(int, row["version"].split("."))))
        runtime = selected["identifier"]
        name = "vibeface-provider-" + run_id + "-" + attempt
        simulator = command(["xcrun", "simctl", "create", name, "com.apple.CoreSimulator.SimDeviceType.iPhone-17", runtime], "create").decode().strip()
        assert re.fullmatch(r"[a-fA-F0-9-]{36}", simulator)
        stage = "simulator-boot"
        command(["xcrun", "simctl", "boot", simulator], "boot")
        command(["xcrun", "simctl", "bootstatus", simulator, "-b"], "bootstatus")
        stage = "runner-prepare"
        result["SDK_prepare_invocations"] = 1
        raw = private / "ios-runner-prepare.raw.json"
        with raw.open("xb") as output, (private / "ios-runner-prepare.stderr.log").open("xb") as error:
            process = subprocess.run(["node", binary, "prepare", "ios-runner", "--platform", "ios", "--udid", simulator, "--json"], cwd=repo, env=environment, stdout=output, stderr=error)
        prepare_code = process.returncode
        result["prepare_exit_code"] = prepare_code
        result["error"] = read_error(repo, raw) if prepare_code else None
        result["status"] = "prepare_failed" if prepare_code else "prepare_command_succeeded"
        runner_tail = b""
        try:
            runner_tail = bounded_file(repo, state / "sessions" / SESSION / "runner.log", tail=True)
            decoded = runner_tail.decode("utf-8", errors="replace")
            result["launch_markers"] = {"status": "tail_captured", "bytes": len(runner_tail), "read_bound_reached": len(runner_tail) == 65536,
                                        "reported": sorted(marker for marker in LAUNCH_MARKERS if "AGENT_DEVICE_RUNNER_" + marker in decoded)}
        except (OSError, ValueError):
            result["launch_markers"] = {"status": "unavailable"}
        try:
            payload = json.dumps({"prepare_result": bounded_file(repo, raw).decode("utf-8", errors="replace"),
                                  "runner_tail": runner_tail.decode("utf-8", errors="replace"), "simulator": simulator}).encode()
            assert len(payload) <= 393216
            program = "import {readFileSync} from 'node:fs'; import {randomBytes,createCipheriv,publicEncrypt,constants} from 'node:crypto'; const key=randomBytes(32),nonce=randomBytes(12),aad=process.argv[2],cipher=createCipheriv('aes-256-gcm',key,nonce); cipher.setAAD(Buffer.from(aad)); const body=Buffer.concat([cipher.update(readFileSync(0)),cipher.final()]); const wrapped=publicEncrypt({key:Buffer.from(process.argv[1],'base64'),oaepHash:'sha256',padding:constants.RSA_PKCS1_OAEP_PADDING},key); process.stdout.write(JSON.stringify({schema:1,aad,nonce:nonce.toString('base64'),tag:cipher.getAuthTag().toString('base64'),key:wrapped.toString('base64'),body:body.toString('base64')}));"
            aad = "vibeface:" + source + ":" + run_id + ":" + attempt + ":runner-launch-v1"
            envelope = root / "public/runner-launch.sealed.json"
            with envelope.open("xb") as output, (private / "seal.stderr").open("xb") as error:
                sealed = subprocess.run(["node", "--input-type=module", "-e", program, 'LS0tLS1CRUdJTiBQVUJMSUMgS0VZLS0tLS0KTUlJQm9qQU5CZ2txaGtpRzl3MEJBUUVGQUFPQ0FZOEFNSUlCaWdLQ0FZRUF2SGxya0JoOEhObWFtMnFnL0lHRQpMSlM0WjNCYi9ZV3VTbjM5N1hZT1ZYeEY0RjNOQTBUUmZJbEdHZkhsa0RtUko3MHN0VnRQeS81blZ0Nk04Z0lzCkRjam0wRlJxa2NONks0cWE4L3V6VWxRK1ZlWTNZa1lBS0YxQ1M2SzY5MXRLQzNqTjdwc1JWVzgwYW10c21XTmUKQmZZc3d0Y01uQ0VRZjAxazkxdjF2Q1pPb2YveFVEbHpCaUU5REU1L1VIOGFXV1pTd3dkQUFqVzJzL2g1eER5UQpzamFiU3BGOG9PaGx5dHFPN1hwNXkrcThiV1pacEEyVjNQWDlINWdQTFlKSWVvM2t1dEorU3dlSzNjYStHVVZHClZheTRYc0NqSE50ZTczRXVoQTN4YkJNYzYwMTJNR1gzZVVDeTVvTGJKYzdMZUdvTnlxcjc1SGFTOFRxRWFpUW8KRWduTmRPak9rbnp5WWlCUjFnU2tjSzQyYnp3b3dZWW5iMjJJbG1MTFQvTVNFUEpZQlNHSjZMUzRuV0owbURjTwpiY3NDbFAwSk1QSDVrZzVCUVduR1NESWNldkU3TVBEb2owTjRaS212RCtKV2ZkOWFqSVowcHFVSHpOMzRKaWdRCi81cHhYRDFXMDlaeXBLY1ZaS2NRbzBEa1dEUDhIK25CR1o5SnhLQWtVUHZ0QWdNQkFBRT0KLS0tLS1FTkQgUFVCTElDIEtFWS0tLS0tCg==', aad], input=payload,
                                        cwd=repo, env=environment, stdout=output, stderr=error, timeout=15)
            if sealed.returncode != 0:
                raise RuntimeError("encryption failed")
            sealed_value = json.loads(envelope.read_bytes())
            assert set(sealed_value) == {"schema", "aad", "nonce", "tag", "key", "body"} and sealed_value["schema"] == 1 and sealed_value["aad"] == aad
            decode = lambda key: __import__("base64").b64decode(sealed_value[key], validate=True)
            assert len(decode("nonce")) == 12 and len(decode("tag")) == 16 and len(decode("key")) == 384 and len(decode("body")) == len(payload)
            result["sealed_launch_evidence"] = {"status": "sealed", "sha256": __import__("hashlib").sha256(envelope.read_bytes()).hexdigest()}
        except (OSError, ValueError, TypeError, KeyError, AssertionError, RuntimeError, subprocess.TimeoutExpired):
            envelope = root / "public/runner-launch.sealed.json"
            if envelope.exists():
                envelope.unlink()
            result["sealed_launch_evidence"] = {"status": "unavailable"}
        toolchain = {"runtime_version": selected["version"] if re.fullmatch(r"[0-9]+(?:\.[0-9]+){1,3}", selected["version"]) else "unknown"}
        for label, arguments in (("xcode-version", ["xcodebuild", "-version"]), ("simulator-sdk-version", ["xcrun", "--sdk", "iphonesimulator", "--show-sdk-version"])):
            try:
                with (private / (label + ".stdout")).open("xb") as output, (private / (label + ".stderr")).open("xb") as error:
                    version = subprocess.run(arguments, cwd=repo, env=environment, stdout=output, stderr=error, timeout=15)
                content = bounded_file(repo, private / (label + ".stdout")).decode("ascii").strip()
                if label == "xcode-version":
                    match = re.fullmatch(r"Xcode ([0-9]+(?:\.[0-9]+){1,3})\nBuild version ([A-Za-z0-9]{1,20})", content)
                    toolchain["xcode"] = {"version": match[1], "build": match[2]} if version.returncode == 0 and match else {"status": "unavailable"}
                else:
                    toolchain["simulator_sdk_version"] = content if version.returncode == 0 and re.fullmatch(r"[0-9]+(?:\.[0-9]+){1,3}", content) else "unknown"
            except (OSError, ValueError, subprocess.TimeoutExpired):
                if label == "xcode-version":
                    toolchain["xcode"] = {"status": "unavailable"}
                else:
                    toolchain["simulator_sdk_version"] = "unknown"
        result["toolchain"] = toolchain
    except (OSError, ValueError, TypeError, AttributeError, KeyError, AssertionError, RuntimeError):
        result["status"] = "probe_failed"
    finally:
        result["stage"] = stage
        if binary is not None and prepare_code is not None:
            try:
                command(["node", binary, "daemon", "stop", "--json"], "daemon-stop")
                cleanup["daemon_stopped"] = True
            except (OSError, RuntimeError):
                pass
        elif result["SDK_prepare_invocations"] == 0:
            cleanup["daemon_not_started"] = True
        if simulator is not None:
            try:
                devices = json.loads(command(["xcrun", "simctl", "list", "devices", "--json"], "cleanup-inventory"))["devices"]
                matches = [row for rows in devices.values() for row in rows if row["udid"] == simulator]
                assert len(matches) == 1 and matches[0]["name"] == "vibeface-provider-" + run_id + "-" + attempt
                shutdown = matches[0]["state"] == "Shutdown"
                if not shutdown:
                    try:
                        command(["xcrun", "simctl", "shutdown", simulator], "shutdown")
                        shutdown = True
                    except (OSError, RuntimeError):
                        pass
                cleanup["simulator_shutdown"] = shutdown
                command(["xcrun", "simctl", "delete", simulator], "delete")
                remaining = json.loads(command(["xcrun", "simctl", "list", "devices", "--json"], "post-delete"))["devices"]
                cleanup["simulator_removed"] = not any(row["udid"] == simulator for rows in remaining.values() for row in rows)
            except (OSError, ValueError, TypeError, AttributeError, KeyError, AssertionError, RuntimeError):
                pass
        else:
            cleanup["simulator_not_created"] = True
        safe = (cleanup["daemon_stopped"] or cleanup.get("daemon_not_started") is True) and (
            cleanup["simulator_removed"] and cleanup.get("simulator_shutdown") is True or cleanup.get("simulator_not_created") is True)
        if safe:
            try:
                for path in (state, private):
                    layout["inside_artifacts"](repo, path)
                    if path.exists():
                        shutil.rmtree(path)
                cleanup["private_state_removed"] = True
            except (OSError, ValueError):
                pass
        result["cleanup"] = cleanup
        result["cleanup_safe"] = safe and cleanup["private_state_removed"]
        (root / "public/runner-probe.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result))
    return 0 if prepare_code == 0 and result["cleanup_safe"] else 1


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    elif sys.argv[1:] == ["--probe"]:
        raise SystemExit(probe())
    else:
        raise SystemExit("usage: python3 scripts/ios-runner-prepare.py --self-test|--probe")
