#!/usr/bin/env python3
"""Install the checksum-pinned guest compiler under .artifacts/vdp/risc0.

Local sandbox runs mount a risc0 home shared by all worktrees there, so the
install lands with one atomic rename and a concurrent loser reuses the winner."""

import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
from unittest import TestCase


ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / ".artifacts/vdp"
RELEASE = "r0.1.97.0"
ASSETS = {
    ("Darwin", "arm64"): ("aarch64-apple-darwin", "85070d82aebbe8e1027e75f2cd0c781f5c82c0ae7e06697218be9d8b71dec15d"),
    ("Linux", "x86_64"): ("x86_64-unknown-linux-gnu", "100b597d605b706bec94e1d32bfaaf5851df8d75173f986d4b56a3e90916be53"),
}

TRANSIENT_HTTP = frozenset({429, 500, 502, 503, 504})
DOWNLOAD_ATTEMPTS = 5
DOWNLOAD_BACKOFF_SECONDS = (2, 4, 8, 16)

_urlopen = urllib.request.urlopen
_sleep = time.sleep


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def transient_download_error(exc):
    if isinstance(exc, urllib.error.HTTPError):
        return exc.code in TRANSIENT_HTTP
    if isinstance(exc, urllib.error.URLError):
        reason = exc.reason
        if isinstance(reason, (TimeoutError, ConnectionResetError, BrokenPipeError)):
            return True
        if isinstance(reason, OSError):
            return reason.errno in (
                errno.ECONNRESET,
                errno.ETIMEDOUT,
                errno.ECONNABORTED,
                errno.EPIPE,
                errno.ENETUNREACH,
                errno.EHOSTUNREACH,
            )
        return False
    if isinstance(exc, (TimeoutError, ConnectionResetError, BrokenPipeError)):
        return True
    if isinstance(exc, OSError):
        return exc.errno in (
            errno.ECONNRESET,
            errno.ETIMEDOUT,
            errno.ECONNABORTED,
            errno.EPIPE,
            errno.ENETUNREACH,
            errno.EHOSTUNREACH,
        )
    return False


def download_pinned_archive(url, destination, expected_digest):
    last_error = None
    for attempt in range(DOWNLOAD_ATTEMPTS):
        destination.unlink(missing_ok=True)
        try:
            with _urlopen(url, timeout=120) as source:
                with destination.open("wb") as output:
                    shutil.copyfileobj(source, output)
            if sha256(destination) != expected_digest:
                destination.unlink(missing_ok=True)
                raise ValueError("Downloaded guest compiler archive checksum mismatch")
            return
        except ValueError:
            raise
        except Exception as exc:
            destination.unlink(missing_ok=True)
            if not transient_download_error(exc) or attempt == DOWNLOAD_ATTEMPTS - 1:
                raise
            last_error = exc
            if attempt < len(DOWNLOAD_BACKOFF_SECONDS):
                _sleep(DOWNLOAD_BACKOFF_SECONDS[attempt])
    if last_error is not None:
        raise last_error
    raise RuntimeError("Guest compiler download failed without an error")


def compiler_root(directory):
    roots = [directory, *(entry for entry in directory.iterdir() if entry.is_dir())]
    matches = [entry for entry in roots if (entry / "bin/rustc").is_file()]
    if len(matches) != 1:
        raise ValueError("Guest compiler archive must contain one bin/rustc at its root or one directory below")
    return matches[0]


def unpack(archive, destination):
    with tarfile.open(archive) as contents:
        for member in contents:
            name = Path(member.name)
            if name.is_absolute() or ".." in name.parts:
                raise ValueError("Unexpected path in checksum-pinned compiler archive")
    subprocess.run(["tar", "-xzf", str(archive), "-C", str(destination)], check=True)
    return compiler_root(destination)


def write_atomic(path, text):
    with tempfile.NamedTemporaryFile("w", prefix=path.name + ".", dir=path.parent, delete=False) as output:
        output.write(text)
    os.chmod(output.name, 0o644)
    os.replace(output.name, path)


def install():
    target, expected = ASSETS.get((platform.system(), platform.machine()), (None, None))
    if target is None:
        raise ValueError("Delivery guest builds support Linux x86_64 and macOS arm64; no toolchain substitution")
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    home = ARTIFACTS / "risc0"
    version = home / f"toolchains/v1.97.0-rust-{target}"
    receipt = version / "installed.json"
    fixed = {"release": RELEASE, "target": target, "archiveSha256": expected}
    if not version.exists():
        name = f"rust-toolchain-{target}.tar.gz"
        archive = ARTIFACTS / name
        if not archive.is_file():
            url = f"https://github.com/risc0/rust/releases/download/{RELEASE}/{name}"
            with tempfile.NamedTemporaryFile(prefix=name + ".", suffix=".download", dir=ARTIFACTS, delete=False) as output:
                temporary = Path(output.name)
            try:
                download_pinned_archive(url, temporary, expected)
                os.replace(temporary, archive)
            finally:
                temporary.unlink(missing_ok=True)
        if sha256(archive) != expected:
            raise ValueError("Guest compiler archive checksum mismatch")
        version.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="guest-install-", dir=version.parent) as directory:
            extracted = unpack(archive, Path(directory))
            (extracted / receipt.name).write_text(json.dumps(fixed) + "\n")
            try:
                os.rename(extracted, version)
            except OSError as error:
                if error.errno not in (errno.ENOTEMPTY, errno.EEXIST):
                    raise
    if version.is_symlink() or not receipt.is_file():
        raise ValueError("Existing compiler directory has no installation receipt")
    if json.loads(receipt.read_text()) != fixed:
        raise ValueError("Existing compiler installation differs from the fixed release")
    if not (version / "bin/rustc").is_file():
        raise ValueError("Existing compiler installation is incomplete")
    write_atomic(home / "settings.toml", '[default_versions]\nrust = "1.97.0"\n')
    result = subprocess.check_output([str(version / "bin/rustc"), "--version"], text=True).strip()
    if not result.startswith("rustc 1.97.0"):
        raise ValueError("Installed guest compiler reports the wrong version")
    print(json.dumps({"risc0Home": str(home), "release": RELEASE, "target": target, "archiveSha256": expected, "compiler": result}))


class _Response:
    def __init__(self, payload):
        self._payload = payload
        self._offset = 0

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def read(self, size=-1):
        if size < 0:
            chunk = self._payload[self._offset :]
            self._offset = len(self._payload)
            return chunk
        chunk = self._payload[self._offset : self._offset + size]
        self._offset += len(chunk)
        return chunk


def _self_test_download_retries():
    global _urlopen, _sleep
    sleeps = []
    opens = []
    root = ARTIFACTS / "installer-download-self-test"
    root.mkdir(parents=True, exist_ok=True)
    original_urlopen = _urlopen
    original_sleep = _sleep

    def track_sleep(seconds):
        sleeps.append(seconds)

    try:
        _sleep = track_sleep
        url = f"https://github.com/risc0/rust/releases/download/{RELEASE}/rust-toolchain-fixture.tar.gz"
        good = b"checksum-pinned-guest-archive-fixture"
        expected = hashlib.sha256(good).hexdigest()

        def open_504_then_ok(open_url, timeout=120):
            opens.append(open_url)
            if len(opens) <= 2:
                raise urllib.error.HTTPError(open_url, 504, "Gateway Time-out", None, None)
            return _Response(good)

        _urlopen = open_504_then_ok
        destination = root / "retry-success.download"
        download_pinned_archive(url, destination, expected)
        assert destination.is_file() and sha256(destination) == expected
        assert len(opens) == 3
        assert sleeps == [2, 4]
        destination.unlink(missing_ok=True)
        opens.clear()
        sleeps.clear()

        def open_always_504(open_url, timeout=120):
            opens.append(open_url)
            raise urllib.error.HTTPError(open_url, 504, "Gateway Time-out", None, None)

        _urlopen = open_always_504
        destination = root / "retry-exhaust.download"
        try:
            download_pinned_archive(url, destination, expected)
        except urllib.error.HTTPError as error:
            assert error.code == 504
        else:
            raise AssertionError("Transient download exhaustion must fail")
        assert not destination.exists()
        assert len(opens) == DOWNLOAD_ATTEMPTS
        assert sum(sleeps) == sum(DOWNLOAD_BACKOFF_SECONDS)
        opens.clear()
        sleeps.clear()

        def open_403(open_url, timeout=120):
            opens.append(open_url)
            raise urllib.error.HTTPError(open_url, 403, "Forbidden", None, None)

        _urlopen = open_403
        destination = root / "no-retry-403.download"
        try:
            download_pinned_archive(url, destination, expected)
        except urllib.error.HTTPError as error:
            assert error.code == 403
        else:
            raise AssertionError("Permanent HTTP errors must not retry")
        assert len(opens) == 1
        assert sleeps == []
        opens.clear()

        def open_bad_digest(open_url, timeout=120):
            opens.append(open_url)
            return _Response(b"wrong-archive-bytes")

        _urlopen = open_bad_digest
        destination = root / "digest-mismatch.download"
        try:
            download_pinned_archive(url, destination, expected)
        except ValueError as error:
            assert "checksum mismatch" in str(error)
        else:
            raise AssertionError("Checksum mismatch must fail without retry")
        assert not destination.exists()
        assert len(opens) == 1
        opens.clear()

        class _BrokenStream(_Response):
            def read(self, size=-1):
                if self._offset == 0:
                    self._offset = 1
                    return b"x"
                raise urllib.error.URLError(ConnectionResetError())

        calls = []

        def open_reset_then_ok(open_url, timeout=120):
            calls.append(open_url)
            if len(calls) == 1:
                return _BrokenStream(good)
            return _Response(good)

        _urlopen = open_reset_then_ok
        destination = root / "partial-retry.download"
        download_pinned_archive(url, destination, expected)
        assert destination.is_file() and sha256(destination) == expected
        assert len(calls) == 2
        assert sleeps == [2]
        destination.unlink(missing_ok=True)
    finally:
        _urlopen = original_urlopen
        _sleep = original_sleep
        shutil.rmtree(root, ignore_errors=True)


def self_test():
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="installer-self-test-", dir=ARTIFACTS) as directory:
        root = Path(directory)
        for nested in (False, True):
            case = root / str(nested)
            compiler = case / ("toolchain/bin/rustc" if nested else "bin/rustc")
            compiler.parent.mkdir(parents=True)
            compiler.write_bytes(b"compiler-layout-fixture")
            assert compiler_root(case) == compiler.parent.parent
        assert sha256(root / "False/bin/rustc") == hashlib.sha256(b"compiler-layout-fixture").hexdigest()
        archive = root / "invalid.tar.gz"
        with tarfile.open(archive, "w:gz") as contents:
            contents.addfile(tarfile.TarInfo("../outside"))
        with TestCase().assertRaises(ValueError, msg='Archive traversal must fail before extraction'):
            unpack(archive, root)
    _self_test_download_retries()
    print("Guest installer layout and checksum self-tests passed; no compiler was downloaded or executed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    self_test() if args.self_test else install()
