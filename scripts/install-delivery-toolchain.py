#!/usr/bin/env python3
"""Install the checksum-pinned guest compiler inside this checkout's artifacts."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / ".artifacts/vdp"
RELEASE = "r0.1.97.0"
ASSETS = {
    ("Darwin", "arm64"): ("aarch64-apple-darwin", "85070d82aebbe8e1027e75f2cd0c781f5c82c0ae7e06697218be9d8b71dec15d"),
    ("Linux", "x86_64"): ("x86_64-unknown-linux-gnu", "100b597d605b706bec94e1d32bfaaf5851df8d75173f986d4b56a3e90916be53"),
}


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


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


def install():
    target, expected = ASSETS.get((platform.system(), platform.machine()), (None, None))
    if target is None:
        raise ValueError("Delivery guest builds support Linux x86_64 and macOS arm64; no toolchain substitution")
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    home = ARTIFACTS / "risc0"
    version = home / f"toolchains/v1.97.0-rust-{target}"
    receipt = version / "installed.json"
    if version.exists():
        if version.is_symlink() or not receipt.is_file():
            raise ValueError("Existing compiler directory has no installation receipt")
        recorded = json.loads(receipt.read_text())
        if recorded != {"release": RELEASE, "target": target, "archiveSha256": expected}:
            raise ValueError("Existing compiler installation differs from the fixed release")
        if not (version / "bin/rustc").is_file():
            raise ValueError("Existing compiler installation is incomplete")
    else:
        name = f"rust-toolchain-{target}.tar.gz"
        archive = ARTIFACTS / name
        if not archive.is_file():
            url = f"https://github.com/risc0/rust/releases/download/{RELEASE}/{name}"
            with tempfile.NamedTemporaryFile(prefix=name + ".", suffix=".download", dir=ARTIFACTS, delete=False) as output:
                temporary = Path(output.name)
                try:
                    with urllib.request.urlopen(url, timeout=120) as source:
                        shutil.copyfileobj(source, output)
                    output.flush()
                    if sha256(temporary) != expected:
                        raise ValueError("Downloaded guest compiler archive checksum mismatch")
                    os.replace(temporary, archive)
                finally:
                    temporary.unlink(missing_ok=True)
        if sha256(archive) != expected:
            raise ValueError("Guest compiler archive checksum mismatch")
        version.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="guest-install-", dir=ARTIFACTS) as directory:
            extracted = unpack(archive, Path(directory))
            os.rename(extracted, version)
        receipt.write_text(json.dumps({"release": RELEASE, "target": target, "archiveSha256": expected}) + "\n")
    (home / "settings.toml").write_text('[default_versions]\nrust = "1.97.0"\n')
    result = subprocess.check_output([str(version / "bin/rustc"), "--version"], text=True).strip()
    if not result.startswith("rustc 1.97.0"):
        raise ValueError("Installed guest compiler reports the wrong version")
    print(json.dumps({"risc0Home": str(home), "release": RELEASE, "target": target, "archiveSha256": expected, "compiler": result}))


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
        try:
            unpack(archive, root)
        except ValueError:
            pass
        else:
            raise AssertionError("Archive traversal must fail before extraction")
    print("Guest installer layout and checksum self-tests passed; no compiler was downloaded or executed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    self_test() if args.self_test else install()
