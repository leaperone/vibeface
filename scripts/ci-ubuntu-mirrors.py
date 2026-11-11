#!/usr/bin/env python3
"""Probe the hosted Ubuntu runner's existing mirrors before optionally reordering them."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit


MIRRORS = Path("/etc/apt/apt-mirrors.txt")
SOURCES = Path("/etc/apt/sources.list.d/ubuntu.sources")
KEYRING = "/usr/share/keyrings/ubuntu-archive-keyring.gpg"
OFFICIAL = {
    "http://azure.archive.ubuntu.com/ubuntu/",
    "https://archive.ubuntu.com/ubuntu/",
    "https://security.ubuntu.com/ubuntu/",
}
PORTS = {"http://ports.ubuntu.com/ubuntu-ports/", "https://ports.ubuntu.com/ubuntu-ports/"}


def source_plan(text, architecture):
    blocks = []
    for number, paragraph in enumerate(re.split(r"\n\s*\n", text.strip())):
        if number < 8:
            public = {}
            for line in paragraph.splitlines():
                key, separator, value = line.partition(":")
                if separator and key in ("Types", "URIs", "Suites", "Signed-By"):
                    value = value.strip()
                    if key == "URIs":
                        value = re.sub(r"(?<=://)[^/\s]*@", "[redacted]@", value)
                        value = re.sub(r"[?#]\S*", "[redacted]", value)
                    public[key] = value[:256]
            print("ubuntu_source=" + json.dumps(public, sort_keys=True), flush=True)
        fields = {}
        for line in paragraph.splitlines():
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            if line[:1].isspace() or ":" not in line:
                raise ValueError("unsupported Ubuntu source field")
            key, value = line.split(":", 1)
            if key in fields:
                raise ValueError("duplicate Ubuntu source field")
            fields[key] = value.strip()
        if fields.get("Enabled", "yes") == "no":
            continue
        types = fields.get("Types", "").split()
        if (fields.get("Signed-By") != KEYRING or "deb" not in types
                or len(types) != len(set(types)) or not set(types) <= {"deb", "deb-src"}):
            raise ValueError("expected the runner's signed Ubuntu deb sources")
        uris = fields.get("URIs", "").split()
        expected = {"mirror+file:/etc/apt/apt-mirrors.txt"} if architecture == "amd64" else PORTS
        if not uris or not set(uris) <= expected:
            raise ValueError("unsupported Ubuntu source URI for this architecture")
        suites = fields.get("Suites", "").split()
        if not suites or any(not re.fullmatch(r"[a-z][a-z0-9-]*", suite) for suite in suites):
            raise ValueError("invalid Ubuntu suites")
        blocks.append((uris, suites))
    if not blocks:
        raise ValueError("no enabled Ubuntu sources")
    suites = sorted({suite for _, names in blocks for suite in names})
    return suites, sorted({uri for uris, _ in blocks for uri in uris})


def mirror_urls(text):
    urls = []
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        fields = line.split()
        if fields[0] not in OFFICIAL or any(not re.fullmatch(r"priority:\d+", item) for item in fields[1:]):
            raise ValueError("unsupported entry in the runner's official mirror list")
        if len(fields) > 2 or fields[0] in urls:
            raise ValueError("duplicate mirror or priority")
        urls.append(fields[0])
    if not urls:
        raise ValueError("empty Ubuntu mirror list")
    return urls


def reordered(text, healthy):
    urls = mirror_urls(text)
    ranks = {url: index + 1 for index, url in enumerate(
        [url for url in urls if url in healthy] + [url for url in urls if url not in healthy]
    )}
    lines = []
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            lines.append(line)
        else:
            url = line.split()[0]
            lines.append(f"{url}\tpriority:{ranks[url]}")
    return "\n".join(lines) + "\n"


def command(args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, text=True, timeout=20, **kwargs)


def probe(url, suites, temporary):
    host = urlsplit(url).hostname
    print(f"mirror={url}", flush=True)
    try:
        addresses = command(["getent", "ahosts", host]).stdout.splitlines()
        print("addresses=" + ",".join(sorted({line.split()[0] for line in addresses})), flush=True)
    except (subprocess.SubprocessError, OSError) as error:
        print(f"dns_failed={type(error).__name__}", flush=True)
        return False
    healthy = True
    for suite in suites:
        release = temporary / "InRelease"
        try:
            command(["curl", "--fail", "--silent", "--show-error", "--max-time", "15",
                     "--proto", "=http,https", "--output", str(release), f"{url}dists/{suite}/InRelease"])
            cleartext = temporary / "verified-release"
            cleartext.unlink(missing_ok=True)
            verified = command(["gpgv", "--keyring", KEYRING, "--output", str(cleartext), str(release)])
            fields = cleartext.read_text()
            if not re.search(rf"^Suite: {re.escape(suite)}$", fields, re.MULTILINE) or not re.search(
                r"^Origin: Ubuntu$", fields, re.MULTILINE
            ):
                raise ValueError("signed release does not match the requested Ubuntu suite")
            print(f"suite={suite} verified=true {verified.stderr.strip()}", flush=True)
        except (subprocess.SubprocessError, OSError, ValueError) as error:
            healthy = False
            detail = error.stderr.strip() if isinstance(error, subprocess.CalledProcessError) else str(error)
            print(f"suite={suite} verified=false error={detail}", flush=True)
    return healthy


def run(apply):
    if not (os.environ.get("GITHUB_ACTIONS") == "true"
            and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
            and os.environ.get("RUNNER_OS") == "Linux"):
        raise ValueError("this command is only for disposable GitHub-hosted Linux runners")
    if 'ID=ubuntu' not in Path("/etc/os-release").read_text().splitlines():
        raise ValueError("expected Ubuntu")
    for tool in ("dpkg", "getent", "curl", "gpgv"):
        if not shutil.which(tool):
            raise ValueError(f"required existing tool unavailable: {tool}")
    architecture = command(["dpkg", "--print-architecture"]).stdout.strip()
    if architecture not in ("amd64", "arm64"):
        raise ValueError("unsupported hosted Ubuntu architecture")
    original_sources = SOURCES.read_text()
    suites, uris = source_plan(original_sources, architecture)
    print(f"architecture={architecture} suites={','.join(suites)} signed_by={KEYRING}", flush=True)
    original = MIRRORS.read_text() if architecture == "amd64" else None
    candidates = mirror_urls(original) if original is not None else uris
    for url in candidates:
        print(f"original_candidate={url}", flush=True)
    with tempfile.TemporaryDirectory(prefix="ci-ubuntu-mirrors-") as directory:
        healthy = {url for url in candidates if probe(url, suites, Path(directory))}
    if not healthy:
        raise ValueError("no existing official mirror passed every suite; configuration is unchanged")
    if architecture == "arm64":
        print("ports_probe_complete=true configuration_unchanged=true", flush=True)
        return
    if not apply:
        print("probe_only=true configuration_unchanged=true", flush=True)
        return
    if MIRRORS.read_text() != original or SOURCES.read_text() != original_sources:
        raise ValueError("APT configuration changed during probing; no write performed")
    updated = reordered(original, healthy)
    command(["sudo", "tee", str(MIRRORS)], input=updated)
    print("applied_priorities=true all_original_candidates_retained=true", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="prioritize verified existing mirrors on this hosted runner")
    args = parser.parse_args()
    try:
        run(args.apply)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Ubuntu mirror probe failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
