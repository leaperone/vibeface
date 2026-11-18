#!/usr/bin/env python3
"""Run the independent source self-tests shared by CI and preflight."""

import argparse
from pathlib import Path
import shlex
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
CHECKS = (
    ("scripts/ci.py", "--self-test"),
    ("scripts/build-inputs.py", "self-test"),
    ("scripts/check-cli-parity.py", "--self-test"),
    ("scripts/check-delivery.py", "--self-test"),
    ("scripts/install-delivery-toolchain.py", "--self-test"),
    ("scripts/run-delivery-live.py", "--self-test"),
    ("scripts/hourly.py", "--self-test"),
    ("scripts/release.py", "self-test"),
    ("scripts/tests/release/test_component_identity.py",),
    ("scripts/tests/release/test_cli.py",),
    ("scripts/tests/release/test_publisher.py",),
    ("scripts/tests/release/test_source.py",),
    ("scripts/release-dispatch.py", "--self-test"),
    ("scripts/check-release-workflow.py", "--self-test"),
    ("scripts/test_release_trains.py",),
    ("scripts/deploy.py", "self-test"),
    ("scripts/deploy-run.py", "self-test"),
    ("scripts/deploy-activate.py", "--self-test"),
    ("deploy/runtime.py", "self-test"),
    ("deploy/static-config.py", "--self-test"),
    ("scripts/package-node-windows.py", "--self-test"),
    ("scripts/checksum.py", "--self-test"),
    ("scripts/ios_device.py", "--self-test"),
    ("scripts/offline-docs.py", "--self-test"),
    ("node", "--test", "contracts/delivery/local-evm-fixture.test.mjs"),
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true", help="list the checks without running them")
    args = parser.parse_args()
    for check in CHECKS:
        if args.list:
            print(shlex.join(check))
            continue
        print(f"Source self-test: {check[0]}", flush=True)
        try:
            command = list(check) if check[0] == "node" else [sys.executable, "-B", *check]
            subprocess.run(command, cwd=ROOT, check=True)
        except subprocess.CalledProcessError as error:
            print(f"Source self-test failed: {check[0]} (exit {error.returncode})", file=sys.stderr)
            return error.returncode
        except OSError as error:
            print(f"Source self-test failed: {check[0]}: {error}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
