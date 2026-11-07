"""Capture a bounded iOS simulator automation actor census after an initial UI failure."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
from typing import Any


UUID = re.compile(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
ACTORS = {
    "vibeface": {"vibeface"},
    "snapshot_bridge": {"snapshot-bridge"},
    "xctest_runner": {"agentdevicerunner", "agentdevicerunner-runner", "xctest", "runner"},
    "node": {"node"},
}


def inside_artifacts(root: Path, candidate: Path) -> Path:
    artifact_root = root / ".artifacts/native"
    if artifact_root.is_symlink() or not artifact_root.is_dir():
        raise ValueError("artifact root cannot be a symlink")
    artifact_root = artifact_root.absolute()
    absolute = candidate.absolute()
    try:
        relative = absolute.relative_to(artifact_root)
    except ValueError as error:
        raise ValueError("diagnostic path is outside task artifacts") from error
    current = artifact_root
    for part in relative.parts:
        if part == ".":
            continue
        current = current / part
        if current.is_symlink():
            raise ValueError("diagnostic path cannot contain symlinks")
    return absolute


def bounded_process_table(text: str) -> dict[str, list[dict[str, Any]]]:
    result: dict[str, list[dict[str, Any]]] = {actor: [] for actor in ACTORS}
    if len(text) > 2_000_000:
        raise ValueError("process table exceeds its bound")
    for line in text.splitlines():
        fields = line.split(None, 3)
        if len(fields) != 4 or not fields[0].isdigit() or not fields[1].isdigit():
            continue
        if not re.fullmatch(r"[A-Za-z<>?+!-]{1,8}", fields[2]):
            continue
        name = Path(fields[3].strip()).name.casefold()
        actor = next((key for key, names in ACTORS.items() if name in names), None)
        if actor is None:
            continue
        result[actor].append({"pid": int(fields[0]), "ppid": int(fields[1]), "state": fields[2]})
    for entries in result.values():
        entries.sort(key=lambda item: item["pid"])
        del entries[8:]
    return result


def actor_process(pid: object, *, run: Any = subprocess.run) -> dict[str, Any]:
    if type(pid) is not int or not 0 < pid <= 2**31 - 1:
        return {"status": "missing"}
    try:
        probe = run(
            ["/bin/ps", "-p", str(pid), "-o", "pid=,ppid=,stat=,comm="],
            capture_output=True,
            text=True,
            timeout=2,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return {"status": "probe_timeout"}
    except OSError:
        return {"status": "probe_unavailable"}
    if probe.returncode != 0:
        return {"status": "not_running"}
    matches = bounded_process_table(probe.stdout)
    for actor, entries in matches.items():
        found = next((entry for entry in entries if entry["pid"] == pid), None)
        if found:
            return {"status": "running", "actor": actor, "state": found["state"]}
    return {"status": "running", "actor": "other"}


def safe_task_record(path: Path, *, kind: str, process_lookup=actor_process) -> dict[str, Any]:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return {"status": "missing"}
    except OSError:
        return {"status": "unreadable"}
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 65536:
        return {"status": "invalid"}
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"status": "invalid"}
    if not isinstance(value, dict):
        return {"status": "invalid"}
    if kind == "daemon":
        pid = value.get("pid")
        transport = value.get("transport")
        ownership = value.get("ownership")
        version = value.get("version")
        plugins = value.get("plugins")
        if type(pid) is not int or not 0 < pid <= 2**31 - 1:
            return {"status": "invalid"}
        return {
            "status": "present",
            "pid": pid,
            "process": process_lookup(pid),
            "transport": transport if transport in {"socket", "http", "dual"} else "unknown",
            "ownership": ownership if ownership in {"detached", "embedded", "unknown"} else "unknown",
            "version": version if isinstance(version, str) and re.fullmatch(r"\d+\.\d+\.\d+", version) else "unknown",
            "plugin_count": len(plugins) if isinstance(plugins, list) and len(plugins) <= 32 else None,
        }
    device_id = value.get("deviceId")
    if device_id is None:
        return {"status": "invalid"}
    owner_pid, runner_pid = value.get("ownerPid"), value.get("runnerPid")
    if type(owner_pid) is not int or not 0 < owner_pid <= 2**31 - 1:
        return {"status": "invalid"}
    if runner_pid is not None and (type(runner_pid) is not int or not 0 < runner_pid <= 2**31 - 1):
        return {"status": "invalid"}
    return {
        "status": "present",
        "owner_pid": owner_pid,
        "runner_pid": runner_pid,
        "device_matches": device_id == os.environ.get("VIBEFACE_E2E_SIMULATOR"),
        "session_matches": value.get("sessionId") == "vdp-native-acceptance",
        "owner_process": process_lookup(owner_pid),
        "runner_process": process_lookup(runner_pid),
    }


def safe_task_directory(path: Path, artifact_root: Path) -> Path:
    if not path.is_absolute():
        raise ValueError("task state directory must be absolute")
    try:
        relative = path.relative_to(artifact_root)
    except ValueError as error:
        raise ValueError("task state directory is outside task artifacts") from error
    current = artifact_root
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            raise ValueError("task state directory cannot contain symlinks")
    if not current.is_dir():
        raise ValueError("task state directory is unavailable")
    return current


def snapshot_cache() -> dict[str, Any]:
    if os.environ.get("CI") != "true":
        return {"status": "skipped_outside_ci", "binary_count": 0}
    home = Path.home()
    agent_home = home / ".agent-device"
    directory = agent_home / "snapshot-source"
    if home.is_symlink() or agent_home.is_symlink():
        return {"status": "invalid", "binary_count": 0}
    try:
        metadata = directory.lstat()
    except FileNotFoundError:
        return {"status": "missing", "binary_count": 0}
    except OSError:
        return {"status": "unreadable", "binary_count": 0}
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        return {"status": "invalid", "binary_count": 0}
    binaries = []
    directories = [directory]
    visited = 0
    while directories and visited < 256:
        current = directories.pop()
        visited += 1
        try:
            entries = list(current.iterdir())
        except OSError:
            continue
        for entry in entries:
            try:
                info = entry.lstat()
            except OSError:
                continue
            if stat.S_ISLNK(info.st_mode):
                continue
            if stat.S_ISDIR(info.st_mode) and len(entry.relative_to(directory).parts) < 4:
                directories.append(entry)
            elif stat.S_ISREG(info.st_mode) and entry.name == "snapshot-bridge":
                binaries.append({"executable": bool(info.st_mode & 0o111), "bytes": min(info.st_size, 100_000_000)})
                if len(binaries) >= 32:
                    directories.clear()
                    break
    return {"status": "present", "binary_count": len(binaries), "binaries": binaries[:8]}


def snapshot_socket_path(command: str, simulator: str) -> tuple[Path, int] | None:
    match = re.search(
        r"(/tmp/agent-device-ax-([a-f0-9]{12})-([1-9][0-9]*)-([a-f0-9]{12})/snapshot\.sock)(?:\s|$)",
        command,
    )
    if match is None:
        return None
    expected_udid_hash = hashlib.sha256(simulator.encode("utf-8")).hexdigest()[:12]
    if match.group(2) != expected_udid_hash:
        raise ValueError("snapshot socket path does not belong to the assigned simulator")
    return Path(match.group(1)), int(match.group(3))


def snapshot_bridge_socket(simulator: str, bridge_pid: int, daemon_pid: int | None, *, run: Any = subprocess.run) -> dict[str, Any]:
    try:
        command = run(
            ["xcrun", "simctl", "spawn", simulator, "ps", "-p", str(bridge_pid), "-o", "command="],
            capture_output=True,
            text=True,
            timeout=3,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return {"status": "command_query_timeout"}
    except OSError:
        return {"status": "command_query_unavailable"}
    if command.returncode != 0 or len(command.stdout) > 8192:
        return {"status": "command_query_failed"}
    try:
        socket = snapshot_socket_path(command.stdout, simulator)
    except ValueError:
        return {"status": "socket_path_not_owned"}
    if socket is None:
        return {"status": "socket_path_not_reported"}
    path, socket_pid = socket
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return {"status": "socket_missing", "socket_owner_pid": socket_pid,
                "socket_owner_matches_agent_device_daemon": socket_pid == daemon_pid if daemon_pid else None}
    except OSError:
        return {"status": "socket_unreadable", "socket_owner_pid": socket_pid,
                "socket_owner_matches_agent_device_daemon": socket_pid == daemon_pid if daemon_pid else None}
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISSOCK(metadata.st_mode):
        return {"status": "socket_wrong_type", "socket_owner_pid": socket_pid,
                "socket_owner_matches_agent_device_daemon": socket_pid == daemon_pid if daemon_pid else None}
    return {
        "status": "socket_present",
        "socket_owner_pid": socket_pid,
        "socket_owner_matches_agent_device_daemon": socket_pid == daemon_pid if daemon_pid else None,
        "bridge_process_pid": bridge_pid,
    }


def simulator_processes(simulator: str, daemon_pid: int | None, *, run: Any = subprocess.run) -> dict[str, Any]:
    try:
        result = run(
            ["xcrun", "simctl", "spawn", simulator, "ps", "-A", "-o", "pid=,ppid=,stat=,comm="],
            capture_output=True,
            text=True,
            timeout=4,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return {"status": "query_timeout"}
    except OSError:
        return {"status": "query_unavailable"}
    if result.returncode != 0:
        return {"status": "query_failed"}
    try:
        actors = bounded_process_table(result.stdout)
    except ValueError:
        return {"status": "output_over_limit"}
    bridges = actors["snapshot_bridge"]
    sockets = [snapshot_bridge_socket(simulator, entry["pid"], daemon_pid, run=run) for entry in bridges[:2]]
    return {"status": "captured", "actors": actors, "bridge_sockets": sockets}


def capture(simulator: str, *, run: Any = subprocess.run) -> dict[str, Any]:
    if not UUID.fullmatch(simulator):
        raise ValueError("simulator identifier is invalid")
    root = Path(__file__).resolve().parents[1]
    artifacts = inside_artifacts(root, root / ".artifacts/native")
    expected = artifacts / "simulator.txt"
    info = expected.lstat()
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode) or info.st_size > 128:
        raise ValueError("simulator marker is invalid")
    if expected.read_text(encoding="utf-8").strip().casefold() != simulator.casefold():
        raise ValueError("simulator is not assigned to this task")
    run_id = os.environ.get("GITHUB_RUN_ID", "")
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "")
    if not run_id.isdigit() or not attempt.isdigit():
        raise ValueError("run identity is invalid")

    devices = run(["xcrun", "simctl", "list", "devices", "--json"], capture_output=True, text=True, timeout=4, check=False)
    if devices.returncode != 0 or len(devices.stdout) > 2_000_000:
        raise ValueError("assigned simulator inventory is unavailable")
    inventory = json.loads(devices.stdout)
    rows = [row for group in inventory.get("devices", {}).values() if isinstance(group, list) for row in group
            if isinstance(row, dict) and row.get("udid", "").casefold() == simulator.casefold()]
    if len(rows) != 1 or rows[0].get("name") != f"vibeface-provider-{run_id}-{attempt}" or rows[0].get("state") != "Booted":
        raise ValueError("assigned simulator identity or state changed")

    daemon_state = safe_task_directory(Path(os.environ.get("AGENT_DEVICE_STATE_DIR", "")), artifacts)
    daemon_record = safe_task_record(daemon_state / "daemon.json", kind="daemon")
    daemon_pid = daemon_record.get("pid") if daemon_record.get("status") == "present" else None
    census: dict[str, Any] = {
        "schema": 1,
        "stage": "initial-accessibility-failure",
        "source_commit": os.environ.get("VIBEFACE_E2E_SOURCE_COMMIT", ""),
        "run_id": run_id,
        "run_attempt": attempt,
        "simulator": {"status": "booted", "app": simulator_processes(simulator, daemon_pid, run=run)},
        "snapshot_bridge_cache": snapshot_cache(),
    }
    census["agent_device_daemon"] = daemon_record
    if os.environ.get("CI") != "true":
        census["xctest_runner"] = {"status": "lease_directory_unavailable"}
    else:
        agent_home = Path.home() / ".agent-device"
        lease_root = Path(os.environ.get("AGENT_DEVICE_IOS_RUNNER_LEASE_DIR", agent_home / "apple-runner/leases"))
        try:
            lease_relative = lease_root.absolute().relative_to(agent_home.absolute())
            current = agent_home
            invalid_path = agent_home.is_symlink()
            for part in lease_relative.parts:
                current = current / part
                invalid_path = invalid_path or current.is_symlink()
        except ValueError:
            invalid_path = True
        if invalid_path or not lease_root.is_dir():
            census["xctest_runner"] = {"status": "lease_directory_unavailable"}
        else:
            lease_path = lease_root / f"{simulator}.json"
            census["xctest_runner"] = safe_task_record(lease_path, kind="runner")
    return census


def self_test() -> None:
    from tempfile import TemporaryDirectory

    table = """  41  1 S snapshot-bridge
  42  41 S /Applications/Vibeface.app/Vibeface
  43  41 R AgentDeviceRunner-Runner
  44  1 S unrelated-secret-command --token=not-captured
"""
    actors = bounded_process_table(table)
    assert actors["snapshot_bridge"] == [{"pid": 41, "ppid": 1, "state": "S"}]
    assert actors["vibeface"] == [{"pid": 42, "ppid": 41, "state": "S"}]
    assert actors["xctest_runner"] == [{"pid": 43, "ppid": 41, "state": "R"}]
    assert actors["node"] == []
    assert "unrelated-secret-command" not in json.dumps(actors)
    simulator = "12345678-1234-1234-1234-123456789abc"
    socket_command = f"snapshot-bridge serve /tmp/agent-device-ax-{hashlib.sha256(simulator.encode()).hexdigest()[:12]}-42-abcdef012345/snapshot.sock --idle-timeout 60"
    socket = snapshot_socket_path(socket_command, simulator)
    assert socket is not None
    socket_path, host_pid = socket
    assert socket_path.name == "snapshot.sock" and host_pid == 42
    try:
        snapshot_socket_path(socket_command, "87654321-4321-4321-4321-cba987654321")
    except ValueError:
        pass
    else:
        raise AssertionError("accepted a socket namespace for another simulator")

    with TemporaryDirectory(prefix="ios-actor-census-") as temporary:
        daemon = Path(temporary) / "daemon.json"
        daemon.write_text(json.dumps({
            "pid": 42, "transport": "socket", "ownership": "detached", "version": "0.21.32",
            "plugins": [], "token": "must-not-escape",
        }), encoding="utf-8")
        projected = safe_task_record(daemon, kind="daemon", process_lookup=lambda _pid: {"status": "running", "actor": "node", "state": "S"})
        encoded = json.dumps(projected)
        assert projected["process"]["actor"] == "node"
        assert "must-not-escape" not in encoded and "token" not in encoded
    print("iOS actor census bounds passed")


def main(argv: list[str]) -> None:
    if argv == ["--self-test"]:
        self_test()
        return
    if len(argv) != 1 or not UUID.fullmatch(argv[0]):
        raise SystemExit("usage: python3 scripts/ios-actor-census.py <assigned-simulator-uuid>")
    root = Path(__file__).resolve().parents[1]
    output_dir = inside_artifacts(root, root / ".artifacts/native/private")
    output_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    census = capture(argv[0])
    output = output_dir / "ios-actor-census.json"
    if output.is_symlink() or output.exists() and not output.is_file():
        raise ValueError("census output must be a regular file")
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=output_dir, prefix="ios-actor-census.", delete=False) as stream:
        os.chmod(stream.name, 0o600)
        stream.write(json.dumps(census, separators=(",", ":")) + "\n")
        temporary = Path(stream.name)
    temporary.replace(output)


if __name__ == "__main__":
    main(sys.argv[1:])
