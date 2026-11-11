"""Capture a bounded iOS simulator automation actor census after an initial UI failure."""

from __future__ import annotations

import hashlib
import itertools
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import time
from typing import Any
from unittest import TestCase


UUID = re.compile(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
ACTORS = {
    "vibeface": {"vibeface"},
    "snapshot_bridge": {"snapshot-bridge"},
    "xctest_runner": {"agentdevicerunner", "agentdevicerunner-runner", "xctest", "runner"},
    "node": {"node"},
}


class ProbeBudget:
    def __init__(self, seconds=14, *, run=subprocess.run, clock=time.monotonic):
        self.clock = clock
        self.deadline = clock() + seconds
        self.runner = run

    def remaining(self):
        return self.deadline - self.clock()

    def run(self, command, **options):
        remaining = self.remaining()
        if remaining <= 0:
            raise subprocess.TimeoutExpired(command, 0)
        options["timeout"] = min(options.get("timeout", remaining), remaining)
        return self.runner(command, **options)


def checked_task_path(root: Path, candidate: Path) -> Path:
    canonical_root = root.resolve()
    absolute = candidate.absolute()
    try:
        relative = absolute.relative_to(canonical_root)
    except ValueError as error:
        raise ValueError("diagnostic path is outside the repository") from error
    current = canonical_root
    for part in relative.parts:
        if part in {"", ".", ".."}:
            raise ValueError("diagnostic path is not normalized")
        current = current / part
        try:
            metadata = current.lstat()
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(metadata.st_mode):
            raise ValueError("diagnostic path cannot contain symlinks")
    if not relative.parts:
        raise ValueError("task path must be inside the repository")
    return absolute


def inside_artifacts(root: Path, candidate: Path) -> Path:
    absolute = checked_task_path(root, candidate)
    if absolute.relative_to(root.resolve()).parts[0] != ".artifacts":
        raise ValueError("diagnostic path is outside task artifacts")
    return absolute


def artifact_layout_safe(repo: Path) -> bool:
    repo = repo.resolve()
    try:
        for relative in (".artifacts/native/private", ".artifacts/native/public", "tests/e2e/.artifacts/native"):
            target = checked_task_path(repo, repo / relative)
            if target.exists() and not target.is_dir():
                return False
        return True
    except (OSError, ValueError):
        return False


def public_census_projection(value: object) -> dict[str, Any]:
    """Return only actor PID/state, socket status/relation, and cache counts."""
    if not isinstance(value, dict) or value.get("schema") != 1:
        raise ValueError("unsupported census")
    simulator = value.get("simulator")
    if not isinstance(simulator, dict) or simulator.get("status") != "booted":
        raise ValueError("invalid simulator census")
    app = simulator.get("app")
    app_states = {"captured", "query_timeout", "query_unavailable", "query_failed", "output_over_limit"}
    if not isinstance(app, dict) or app.get("status") not in app_states:
        raise ValueError("invalid app census")
    projected_app: dict[str, Any] = {"status": app["status"]}
    if app["status"] == "captured":
        actors = app.get("actors")
        if not isinstance(actors, dict) or set(actors) != set(ACTORS):
            raise ValueError("invalid actor census")
        projected_actors = {}
        for actor, entries in actors.items():
            if not isinstance(entries, list) or len(entries) > 8:
                raise ValueError("invalid actor list")
            projected = []
            for entry in entries:
                if not isinstance(entry, dict):
                    raise ValueError("invalid actor")
                pid, state = entry.get("pid"), entry.get("state")
                if type(pid) is not int or not 0 < pid <= 2**31 - 1:
                    raise ValueError("invalid actor PID")
                if not isinstance(state, str) or not re.fullmatch(r"[A-Za-z<>?+!-]{1,8}", state):
                    raise ValueError("invalid actor state")
                projected.append({"pid": pid, "state": state})
            projected_actors[actor] = projected
        sockets = app.get("bridge_sockets")
        socket_states = {"command_query_timeout", "command_query_unavailable", "command_query_failed",
                         "socket_path_not_owned", "socket_path_not_reported", "socket_missing",
                         "socket_unreadable", "socket_wrong_type", "socket_present"}
        if not isinstance(sockets, list) or len(sockets) > 2:
            raise ValueError("invalid socket census")
        projected_sockets = []
        for socket in sockets:
            if not isinstance(socket, dict) or socket.get("status") not in socket_states:
                raise ValueError("invalid socket state")
            relation = socket.get("socket_owner_matches_agent_device_daemon")
            if relation is not None and type(relation) is not bool:
                raise ValueError("invalid socket relation")
            item = {"status": socket["status"]}
            if relation is not None:
                item["daemon_owner_matches"] = relation
            projected_sockets.append(item)
        projected_app.update(actors=projected_actors, bridge_sockets=projected_sockets)
    cache = value.get("snapshot_bridge_cache")
    cache_states = {"present", "missing", "unreadable", "invalid", "skipped_outside_ci", "budget_exhausted"}
    if not isinstance(cache, dict) or cache.get("status") not in cache_states:
        raise ValueError("invalid cache census")
    count = cache.get("binary_count")
    if type(count) is not int or not 0 <= count <= 32:
        raise ValueError("invalid cache count")
    return {
        "status": "captured",
        "simulator": {"status": "booted", "app": projected_app},
        "snapshot_bridge_cache": {"status": cache["status"], "binary_count": count},
    }


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
    if any(part in {".", ".."} for part in relative.parts):
        raise ValueError("task state directory is not normalized")
    current = artifact_root
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            raise ValueError("task state directory cannot contain symlinks")
    if not current.is_dir():
        raise ValueError("task state directory is unavailable")
    return current


def safe_home_directory(path: Path) -> bool:
    home = Path.home().absolute()
    candidate = path.absolute()
    if home.is_symlink():
        return False
    try:
        relative = candidate.relative_to(home)
    except ValueError:
        return False
    if any(part in {".", ".."} for part in relative.parts):
        return False
    current = home
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            return False
    return True


def snapshot_cache(budget: ProbeBudget | None = None) -> dict[str, Any]:
    if os.environ.get("CI") != "true":
        return {"status": "skipped_outside_ci", "binary_count": 0}
    home = Path.home()
    agent_home = home / ".agent-device"
    directory = agent_home / "snapshot-source"
    if not safe_home_directory(directory):
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
    entries_seen = 0
    while directories and visited < 32 and entries_seen < 128:
        if budget is not None and budget.remaining() <= 0:
            return {"status": "budget_exhausted", "binary_count": len(binaries)}
        current = directories.pop()
        visited += 1
        try:
            entries = list(itertools.islice(current.iterdir(), 128 - entries_seen))
        except OSError:
            continue
        for entry in entries:
            entries_seen += 1
            if budget is not None and budget.remaining() <= 0:
                return {"status": "budget_exhausted", "binary_count": len(binaries)}
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
    budget = ProbeBudget(run=run)
    run = budget.run
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
    process_lookup = lambda pid: actor_process(pid, run=run)
    daemon_record = safe_task_record(daemon_state / "daemon.json", kind="daemon", process_lookup=process_lookup)
    daemon_pid = daemon_record.get("pid") if daemon_record.get("status") == "present" else None
    census: dict[str, Any] = {
        "schema": 1,
        "stage": "initial-accessibility-failure",
        "source_commit": os.environ.get("VIBEFACE_E2E_SOURCE_COMMIT", ""),
        "run_id": run_id,
        "run_attempt": attempt,
        "simulator": {"status": "booted", "app": simulator_processes(simulator, daemon_pid, run=run)},
        "snapshot_bridge_cache": snapshot_cache(budget),
    }
    census["agent_device_daemon"] = daemon_record
    if os.environ.get("CI") != "true":
        census["xctest_runner"] = {"status": "lease_directory_unavailable"}
    else:
        agent_home = Path.home() / ".agent-device"
        lease_root = Path(os.environ.get("AGENT_DEVICE_IOS_RUNNER_LEASE_DIR", agent_home / "apple-runner/leases"))
        if not safe_home_directory(lease_root) or not lease_root.is_dir():
            census["xctest_runner"] = {"status": "lease_directory_unavailable"}
        else:
            lease_path = lease_root / f"{simulator}.json"
            census["xctest_runner"] = safe_task_record(lease_path, kind="runner", process_lookup=process_lookup)
    return census



def write_private_census(root: Path, census: dict[str, Any]) -> Path:
    output_dir = inside_artifacts(root, root.resolve() / ".artifacts/native/private")
    output_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    output = inside_artifacts(root, output_dir / "ios-actor-census.json")
    if output.is_symlink() or output.exists() and not output.is_file():
        raise ValueError("census output must be a regular file")
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=output_dir, prefix="ios-actor-census.", delete=False) as stream:
        os.chmod(stream.name, 0o600)
        stream.write(json.dumps(census, separators=(",", ":")) + "\n")
        temporary = Path(stream.name)
    temporary.replace(output)
    os.chmod(output, 0o600)
    return output

def self_test() -> None:
    ticks = iter([0.0, 1.0, 13.5, 14.1])
    probe_timeouts = []
    budget = ProbeBudget(run=lambda command, **options: probe_timeouts.append(options["timeout"]),
                         clock=lambda: next(ticks))
    budget.run(["fixture"], timeout=4)
    budget.run(["fixture"], timeout=4)
    with TestCase().assertRaises(subprocess.TimeoutExpired, msg='an exhausted probe budget started another process'):
        budget.run(["fixture"], timeout=4)
    assert probe_timeouts == [4, 0.5]
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
    with TestCase().assertRaises(ValueError, msg='accepted a socket namespace for another simulator'):
        snapshot_socket_path(socket_command, "87654321-4321-4321-4321-cba987654321")

    secret = "unknown-raw-secret-path-token-uuid-args-must-drop"
    fixture = {
        "schema": 1, "stage": "private-stage", "source_commit": secret,
        "simulator": {"status": "booted", "extra": secret, "app": {
            "status": "captured", "command": secret,
            "actors": {actor: ([{"pid": 42, "ppid": 1, "state": "S", "args": secret}] if actor == "snapshot_bridge" else [])
                       for actor in ACTORS},
            "bridge_sockets": [{"status": "socket_present", "socket_owner_pid": 42,
                                "socket_owner_matches_agent_device_daemon": True, "path": secret}],
        }},
        "snapshot_bridge_cache": {"status": "present", "binary_count": 1, "binaries": [secret]},
        "agent_device_daemon": {"version": secret, "transport": secret, "ownership": secret, "plugin_count": 8},
        "xctest_runner": {"owner_pid": 42, "device_matches": True, "session_matches": True},
    }
    public = public_census_projection(fixture)
    encoded = json.dumps(public)
    assert public["simulator"]["app"]["actors"]["snapshot_bridge"] == [{"pid": 42, "state": "S"}]
    assert public["simulator"]["app"]["bridge_sockets"] == [{"status": "socket_present", "daemon_owner_matches": True}]
    assert public["snapshot_bridge_cache"] == {"status": "present", "binary_count": 1}
    assert secret not in encoded and "version" not in encoded and "transport" not in encoded

    with TemporaryDirectory(prefix="ios-actor-census-") as temporary:
        root = Path(temporary) / "repo"
        private = root / ".artifacts/native/private"
        private.mkdir(parents=True)
        output = write_private_census(root, fixture)
        assert output.is_relative_to(root.resolve())
        assert stat.S_IMODE(output.stat().st_mode) == 0o600
        assert artifact_layout_safe(root)

        for relative in (".artifacts/native/public", "tests/e2e/.artifacts"):
            guarded = Path(temporary) / ("layout-" + relative.replace("/", "-"))
            link = guarded / relative
            link.parent.mkdir(parents=True)
            link.symlink_to(Path(temporary) / "outside-layout", target_is_directory=True)
            assert not artifact_layout_safe(guarded)
        fixture["snapshot_bridge_cache"] = {"status": "budget_exhausted", "binary_count": 0}
        assert public_census_projection(fixture)["snapshot_bridge_cache"] == {"status": "budget_exhausted", "binary_count": 0}

        outside = Path(temporary) / "outside"
        root2 = Path(temporary) / "repo-with-parent-link"
        root2.mkdir()
        root2.joinpath(".artifacts").symlink_to(outside, target_is_directory=True)
        original_lstat = Path.lstat
        def reject_outside_lstat(path):
            if path == outside or outside in path.parents:
                raise AssertionError("followed parent artifact symlink")
            return original_lstat(path)
        Path.lstat = reject_outside_lstat
        try:
            with TestCase().assertRaises(ValueError, msg='accepted a parent artifact symlink'):
                inside_artifacts(root2, root2 / ".artifacts/native/private")
        finally:
            Path.lstat = original_lstat
        assert not outside.exists()
    print("iOS actor census bounds passed")


def main(argv: list[str]) -> None:
    if argv == ["--self-test"]:
        self_test()
        return
    if len(argv) != 1 or not UUID.fullmatch(argv[0]):
        raise SystemExit("usage: python3 scripts/ios-actor-census.py <assigned-simulator-uuid>")
    root = Path(__file__).resolve().parents[1]
    inside_artifacts(root, root / ".artifacts/native/private")
    census = capture(argv[0])
    write_private_census(root, census)


if __name__ == "__main__":
    main(sys.argv[1:])
