#!/usr/bin/env python3
"""Run local Rust checks with the current Vibeface checkout's build directories."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]


def local_environment():
    target = ROOT / "target"
    if target.is_symlink():
        raise ValueError(f"Use a checkout-local directory for {target}")

    env = os.environ.copy()
    # Cargo fingerprints can reuse a path crate across checkouts even when only build-dir is shared.
    for key in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"):
        env[key] = str(target)
    env.setdefault("CARGO_PROFILE_DEV_DEBUG", "line-tables-only")
    env.setdefault("CARGO_PROFILE_TEST_DEBUG", "line-tables-only")
    if (ROOT / ".git").is_file():
        env.setdefault("CARGO_INCREMENTAL", "0")

    if env.get("VIBEFACE_SCCACHE") == "1":
        wrapper = shutil.which("sccache")
        if wrapper is None:
            raise ValueError("VIBEFACE_SCCACHE=1 requires sccache on PATH")
        env["RUSTC_WRAPPER"] = wrapper
        env["CARGO_INCREMENTAL"] = "0"
        env.setdefault("SCCACHE_CACHE_SIZE", "20G")
        env.pop("SCCACHE_BASEDIRS", None)
    return env


def verify_web(env):
    manifests = {"vibeface": ROOT / "apps/cli/Cargo.toml", "vibeface-core": ROOT / "apps/core/Cargo.toml"}
    build = ["cargo", "build", "--locked", "--message-format=json-render-diagnostics"]
    for name in manifests:
        build.extend(("-p", name))
    result = subprocess.run(build, cwd=ROOT, env=env, stdout=subprocess.PIPE, text=True, check=True)
    binaries = {}
    for line in result.stdout.splitlines():
        event = json.loads(line)
        if event.get("reason") != "compiler-artifact" or not event.get("executable"):
            continue
        target = event["target"]
        name = target["name"]
        manifest = Path(event["manifest_path"]).resolve()
        if target["kind"] == ["bin"] and manifest == manifests.get(name):
            binaries[name] = Path(event["executable"]).resolve(strict=True)
    if binaries.keys() != manifests.keys():
        raise ValueError("Cargo did not report the CLI and Core executables for this checkout")

    app = ROOT / "apps/app/dist/client"
    return subprocess.run([
        sys.executable, "scripts/verify-node.py", "--web-only",
        "--binary", str(binaries["vibeface"]), "--core-binary", str(binaries["vibeface-core"]),
        "--app-dir", str(app), "--crypto-module", str(app / "vibeface_secure.js"),
        "--artifacts", ".artifacts/preflight-web",
    ], cwd=ROOT, env=env).returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="mode", required=True)
    run = commands.add_parser("run", help="run a command from the checkout root with the local Rust settings")
    run.add_argument("command", nargs=argparse.REMAINDER)
    commands.add_parser("verify-web", help="build CLI and Core and check the prepared Web assets with them")
    args = parser.parse_args()
    if args.mode == "run" and not args.command:
        run.error("expected a command to run")

    try:
        env = local_environment()
        if args.mode == "verify-web":
            return verify_web(env)
        return subprocess.run(args.command, cwd=ROOT, env=env).returncode
    except subprocess.CalledProcessError as error:
        return error.returncode
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
