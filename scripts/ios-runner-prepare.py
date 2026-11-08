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



def bounded_file(root, path):
    relative = path.absolute().relative_to(root.absolute())
    descriptors = [os.open(root.resolve(), os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)]
    try:
        if not relative.parts or any(part in {"", ".", ".."} for part in relative.parts):
            raise ValueError("invalid task file")
        for part in relative.parts[:-1]:
            descriptors.append(os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptors[-1]))
        descriptors.append(os.open(relative.parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=descriptors[-1]))
        info = os.fstat(descriptors[-1])
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_size > LIMIT:
            raise ValueError("invalid task file")
        data = os.read(descriptors[-1], LIMIT + 1)
        if len(data) > LIMIT:
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


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    else:
        raise SystemExit("usage: python3 scripts/ios-runner-prepare.py --self-test")
