"""Owned POSIX process lifecycle primitives for verification entry points.

The caller supplies the platform-specific terminate/kill operation (for example,
a private POSIX process group).  Windows verifiers keep their handle and identity
checks separate because those semantics cannot be represented by this API.
"""

from __future__ import annotations

import subprocess
from typing import Callable


def stop_owned_process(
    process: subprocess.Popen | None,
    *,
    terminate: Callable[[], object] | None = None,
    kill: Callable[[], object] | None = None,
    wait_timeout: float = 5,
    kill_wait_timeout: float | None = None,
    return_on_terminate_error: bool = False,
) -> None:
    """Stop one process already owned by the caller and reap it.

    The process is first polled to avoid signalling an already exited child.  A
    race with exit is harmless; after a timeout the caller's kill operation is
    used and the second wait is allowed to raise unchanged.  Signal/group policy
    remains in the caller, so this primitive never targets an unrelated process.
    """
    if process is None:
        return
    if process.poll() is None:
        try:
            (terminate or process.terminate)()
        except ProcessLookupError:
            if return_on_terminate_error:
                return
    try:
        process.wait(timeout=wait_timeout)
    except subprocess.TimeoutExpired:
        try:
            (kill or process.kill)()
        except ProcessLookupError:
            pass
        process.wait(timeout=wait_timeout if kill_wait_timeout is None else kill_wait_timeout)
