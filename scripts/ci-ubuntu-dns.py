#!/usr/bin/env python3
"""Repair an unavailable resolver only on disposable hosted Ubuntu package runners."""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


def command(arguments, **kwargs):
    return subprocess.run(arguments, check=True, capture_output=True, text=True, timeout=20, **kwargs)


def resolves(host):
    try:
        result = subprocess.run(["getent", "ahosts", host], capture_output=True, text=True, timeout=20)
    except subprocess.TimeoutExpired:
        print(f"ubuntu_dns_host={host} resolved=false lookup_timeout=true", flush=True)
        return False
    print(f"ubuntu_dns_host={host} resolved={result.returncode == 0}", flush=True)
    return result.returncode == 0


def run(repair):
    if not (os.environ.get("GITHUB_ACTIONS") == "true"
            and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
            and os.environ.get("RUNNER_OS") == "Linux"):
        raise ValueError("resolver checks are restricted to disposable GitHub-hosted Linux runners")
    if "ID=ubuntu" not in Path("/etc/os-release").read_text().splitlines():
        raise ValueError("expected Ubuntu")
    for tool in ("dpkg", "getent", "ip", "resolvectl"):
        if not shutil.which(tool):
            raise ValueError(f"required existing tool unavailable: {tool}")
    architecture = command(["dpkg", "--print-architecture"]).stdout.strip()
    hosts = {"amd64": ("azure.archive.ubuntu.com", "archive.ubuntu.com", "security.ubuntu.com"),
             "arm64": ("ports.ubuntu.com",)}.get(architecture)
    if hosts is None:
        raise ValueError("unsupported hosted Ubuntu architecture")
    if any(resolves(host) for host in hosts):
        print("system_resolver_available=true configuration_unchanged=true", flush=True)
        return
    if not repair:
        raise ValueError("the configured Ubuntu package hosts cannot be resolved; configuration unchanged")
    route = json.loads(command(["ip", "-json", "route", "get", "1.1.1.1"]).stdout)
    if len(route) != 1 or not re.fullmatch(r"[A-Za-z0-9_.:-]{1,64}", route[0].get("dev", "")):
        raise ValueError("cannot identify this hosted runner's outbound interface")
    interface = route[0]["dev"]
    original = command(["resolvectl", "dns", interface]).stdout.partition(":")[2].split()
    for address in original:
        ipaddress.ip_address(address.split("%", 1)[0])
    print("temporary_hosted_dns_repair=true servers=1.1.1.1,8.8.8.8", flush=True)
    try:
        command(["sudo", "resolvectl", "dns", interface, "1.1.1.1", "8.8.8.8"])
        if not any(resolves(host) for host in hosts):
            raise ValueError("public DNS did not resolve any configured Ubuntu package host")
    except (OSError, ValueError, subprocess.SubprocessError):
        if original:
            command(["sudo", "resolvectl", "dns", interface, *original])
        else:
            command(["sudo", "resolvectl", "revert", interface])
        print("previous_interface_resolver_restored=true", flush=True)
        raise
    print("temporary_hosted_dns_verified=true apt_sources_and_authentication_unchanged=true", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repair", action="store_true", help="repair unavailable DNS for this disposable runner")
    args = parser.parse_args()
    try:
        run(args.repair)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Hosted Ubuntu DNS check failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
