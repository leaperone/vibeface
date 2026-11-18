"""Small platform-neutral primitives used by local verification entry points.

This module deliberately contains no process or socket policy of its own.  Callers
choose transient predicate exceptions, timeout diagnostics, the protocols a port
must reserve, and the signal/handle operation used to stop an owned process.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import uuid
from typing import Callable, Sequence


Predicate = Callable[[], object]
TimeoutSuffix = Callable[[BaseException | None], str]


def wait_for_deadline(
    description: str,
    predicate: Predicate,
    timeout: float = 45,
    *,
    transient_exceptions: tuple[type[BaseException], ...] = (),
    interval: float = 0.15,
    timeout_suffix: TimeoutSuffix | None = None,
) -> object:
    """Poll until *predicate* returns a truthy value or the deadline expires.

    Only exception classes explicitly supplied by the caller are treated as
    transient.  Any other exception, including an assertion from the business
    check, propagates immediately.  ``timeout_suffix`` receives the last
    swallowed exception so each adapter can preserve its diagnostic format.
    """
    deadline = time.monotonic() + timeout
    last_error: BaseException | None = None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except transient_exceptions as error:
            last_error = error
        time.sleep(interval)
    suffix = timeout_suffix(last_error) if timeout_suffix is not None else ""
    raise AssertionError(f"timed out waiting for {description}{suffix}")


def stream_sha256(path: Path, *, block_size: int = 1024 * 1024) -> str:
    """Hash a file without loading it into memory."""
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for block in iter(lambda: source.read(block_size), b""):
            digest.update(block)
    return digest.hexdigest()


def private_json(path: Path, value: object) -> None:
    """Atomically replace a private JSON file with mode 0600 contents."""
    path = Path(path)
    temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}")
    try:
        descriptor = os.open(
            temporary,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL,
            0o600,
        )
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as output:
            output.write(json.dumps(value, indent=2) + "\n")
        os.replace(temporary, path)
    except BaseException:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass
        raise


def reserve_loopback_port(
    *,
    protocols: Sequence[str] = ("tcp",),
    host: str = "127.0.0.1",
    requested: int = 0,
    assigned: set[int] | None = None,
) -> int:
    """Reserve one loopback port for the requested TCP/UDP protocols.

    ``requested`` may pin a starting range (the next candidate is tried after a
    collision); zero asks the OS for an ephemeral port.  Sockets are closed once
    the reservation is checked, matching the existing verifier behavior.
    """
    normalized = tuple(protocols)
    if not normalized or any(protocol not in {"tcp", "udp"} for protocol in normalized):
        raise ValueError("protocols must contain only tcp or udp")
    if len(set(normalized)) != len(normalized):
        raise ValueError("protocols must be unique")
    assigned = assigned if assigned is not None else set()
    candidate = int(requested)
    while True:
        sockets: list[socket.socket] = []
        try:
            selected: int | None = None
            for protocol in normalized:
                kind = socket.SOCK_STREAM if protocol == "tcp" else socket.SOCK_DGRAM
                stream = socket.socket(socket.AF_INET, kind)
                sockets.append(stream)
                stream.bind((host, candidate if selected is None else selected))
                if selected is None:
                    selected = stream.getsockname()[1]
            assert selected is not None
            if selected not in assigned:
                assigned.add(selected)
                return selected
        except OSError:
            pass
        finally:
            for stream in sockets:
                stream.close()
        if candidate:
            candidate = (selected + 1) if selected is not None else candidate + 1


def stop_owned_process(
    process: subprocess.Popen,
    *,
    terminate: Callable[[], object] | None = None,
    kill: Callable[[], object] | None = None,
    wait_timeout: float = 5,
) -> None:
    """Stop one already-owned process, keeping group/handle policy in adapters."""
    if process is None:
        return
    try:
        if process.poll() is None:
            (terminate or process.terminate)()
        process.wait(timeout=wait_timeout)
    except ProcessLookupError:
        return
    except subprocess.TimeoutExpired:
        (kill or process.kill)()
        process.wait(timeout=wait_timeout)


def self_test() -> dict[str, object]:
    """Offline checks for the shared primitives; no child or network service."""
    import tempfile

    calls = [0]

    def eventually_ready():
        calls[0] += 1
        if calls[0] == 1:
            raise ValueError("transient fixture")
        return "ready"

    result = wait_for_deadline(
        "offline fixture", eventually_ready, timeout=1, interval=0,
        transient_exceptions=(ValueError,),
        timeout_suffix=lambda error: f" ({type(error).__name__})" if error else "",
    )
    if result != "ready":
        raise AssertionError("deadline wait returned the wrong value")

    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "private.json"
        private_json(path, {"ok": True})
        if path.stat().st_mode & 0o077 or json.loads(path.read_text()) != {"ok": True}:
            raise AssertionError("private JSON is not private and complete")
        if stream_sha256(path) != hashlib.sha256(path.read_bytes()).hexdigest():
            raise AssertionError("stream hash mismatch")

    reserved: set[int] = set()
    tcp = reserve_loopback_port(protocols=("tcp",), assigned=reserved)
    tcp_udp = reserve_loopback_port(protocols=("tcp", "udp"), assigned=reserved)
    if tcp == tcp_udp:
        raise AssertionError("port reservations did not remain distinct")
    return {"status": "passed", "checks": ["deadline", "private_json", "stream_sha256", "tcp_port", "tcp_udp_port"]}


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        print(json.dumps(self_test(), sort_keys=True))
    else:
        parser.error("--self-test is required")
