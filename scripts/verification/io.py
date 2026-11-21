"""Small bounded file, JSON, and hashing primitives for verification entrypoints.

This module deliberately has no dependency on an entrypoint or on scenario
code.  Callers keep their existing validation and error types while sharing
the byte-level operations here.
"""

import hashlib
import json
import os
from pathlib import Path
import stat
import uuid


def digest_bytes(data):
    """Return the hexadecimal SHA-256 digest of bytes."""
    return hashlib.sha256(data).hexdigest()


def digest_file(path, *, block_size=1024 * 1024):
    """Hash a file incrementally without loading it into memory."""
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(block_size), b""):
            value.update(block)
    return value.hexdigest()


def read_bounded(path, limit, *, private=False, error=ValueError):
    """Read a bounded regular file, refusing symlinks and oversized input."""
    path = Path(path)
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= limit:
        raise error("bounded_regular_file_required")
    if private and (info.st_uid != os.getuid() or info.st_mode & 0o077):
        raise error("owner_only_file_required")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as stream:
        data = stream.read(limit + 1)
    if not 0 < len(data) <= limit:
        raise error("bounded_file_read_required")
    return data


def regular(path, limit=1024 * 1024):
    """Compatibility bounded regular-file reader used by live checks."""
    path = Path(path)
    if path.is_symlink() or not path.is_file() or not 0 < path.stat().st_size <= limit:
        raise ValueError("Expected a bounded regular input file")
    return path.read_bytes()


def write_private(path, value):
    """Create a private JSON file, preserving exclusive-create semantics."""
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as output:
        json.dump(value, output, indent=2)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())


def atomic_write(path, data, *, allowed_root=None, error=ValueError):
    """Durably replace a file with bytes, including its parent directory."""
    path = Path(path)
    if allowed_root is not None and not path.parent.resolve().is_relative_to(Path(allowed_root).resolve()):
        raise error("write_outside_task_artifacts")
    temporary = path.with_name("." + path.name + "." + uuid.uuid4().hex)
    with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def save_json(path, value, *, allowed_root=None, error=ValueError):
    atomic_write(path, (json.dumps(value, indent=2, sort_keys=True) + "\n").encode(), allowed_root=allowed_root, error=error)


def replace_public(path, value):
    """Atomically publish a world-readable JSON projection."""
    path = Path(path)
    temporary = path.with_name("." + path.name + "." + uuid.uuid4().hex)
    write_private(temporary, value)
    temporary.chmod(0o644)
    os.replace(temporary, path)
