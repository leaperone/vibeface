#!/usr/bin/env python3
"""Bind generated Web/WASM files to their source inputs and exact output set."""
import argparse
import base64
import fnmatch
import hashlib
import json
import shutil
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
RECEIPT = ".vibeface-build.json"
OUTPUTS = {"crypto": "apps/app/generated", "web": "apps/app/dist/client"}
COMMON = {".gitignore", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain", "scripts/build-inputs.py"}
INPUTS = {
    "crypto": ("crates/secure/", "scripts/build-web-crypto.sh", "scripts/release-rustflags.py"),
    "web": ("apps/app/", "packages/client/", "packages/theme/", "package.json",
            "pnpm-lock.yaml", "pnpm-workspace.yaml", "scripts/build-web.sh"),
}
CACHE_POLICIES = (
    ("/", "no-store"),
    ("/_shell.html", "no-store"),
    ("/index.html", "no-store"),
    ("/vibeface_secure.js", "no-store"),
    ("/vibeface_secure_bg.wasm", "no-store"),
    ("/sw.js", "no-cache"),
    ("/manifest.webmanifest", "no-cache"),
    ("/icon.svg", "no-cache"),
    ("/assets/*", "public, max-age=31536000, immutable"),
)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def content_security_policy(shell):
    hashes = []
    rest = shell
    while "<script" in rest:
        _, script = rest.split("<script", 1)
        try:
            opening, content = script.split(">", 1)
            body, rest = content.split("</script>", 1)
        except ValueError as error:
            raise ValueError("web shell contains an unclosed script tag") from error
        if "src=" not in opening:
            hashes.append("'sha256-" + base64.b64encode(hashlib.sha256(body.encode()).digest()).decode() + "'")
    policy = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'"
    if hashes:
        policy += " " + " ".join(hashes)
    return policy + "; style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data: blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"


def static_headers(policy):
    # Netlify concatenates repeated Cache-Control fields, so the security wildcard has no cache rule.
    lines = [
        "/*",
        "  Content-Security-Policy: " + policy,
        "  X-Content-Type-Options: nosniff",
        "  X-Frame-Options: DENY",
        "  Referrer-Policy: no-referrer",
    ]
    for path, cache in CACHE_POLICIES:
        lines.extend((path, "  Cache-Control: " + cache))
    return "\n".join(lines) + "\n"


def files(directory):
    if directory.is_symlink() or not directory.is_dir():
        raise ValueError(f"missing regular build directory: {directory.name}")
    result = {}
    for path in sorted(directory.rglob("*")):
        if path.is_symlink():
            raise ValueError("generated files must not contain symlinks")
        if path.is_file() and path != directory / RECEIPT:
            result[path.relative_to(directory).as_posix()] = digest(path.read_bytes())
    if not result:
        raise ValueError("generated output is empty")
    return result


def sources(repo, component):
    if component == "web":
        public = repo / "apps/app/public"
        tracked = set(subprocess.check_output(
            ["git", "ls-files", "-z", "--", "apps/app/public"], cwd=repo
        ).decode().split("\0"))
        if public.is_symlink():
            raise ValueError("public assets must not contain symlinks")
        for path in public.rglob("*"):
            if path.is_symlink() or (path.is_file() and path.relative_to(repo).as_posix() not in tracked):
                raise ValueError("public assets must be regular tracked files")
    names = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=repo
    ).decode().split("\0")
    result = {}
    for name in sorted(set(names) - {""}):
        if name == "apps/app/src/routeTree.gen.ts":
            continue
        if name not in COMMON and not any(name == item or name.startswith(item)
                                          for item in INPUTS[component]):
            continue
        path = repo / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"build input must be a regular file: {name}")
        result[name] = digest(path.read_bytes())
    if not result:
        raise ValueError("build source inputs are missing")
    return result


def snapshot(repo, component):
    result = {"source": sources(repo, component)}
    if component == "web":
        check(repo, "crypto")
        result["crypto"] = digest((repo / OUTPUTS["crypto"] / RECEIPT).read_bytes())
    return result


def pending(repo, component):
    return repo / ".artifacts/build-inputs" / f"{component}.json"


def prepare(repo, component):
    inputs = snapshot(repo, component)
    output = repo / OUTPUTS[component]
    cleanup = output.parent if component == "web" else output
    if cleanup.is_symlink() or not cleanup.resolve().is_relative_to(repo.resolve()):
        raise ValueError("build output must stay inside this checkout")
    if cleanup.exists():
        shutil.rmtree(cleanup)
    output.mkdir(parents=True)
    state = pending(repo, component)
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text(json.dumps(inputs, sort_keys=True) + "\n")


def finish(repo, component):
    state = pending(repo, component)
    inputs = json.loads(state.read_text())
    if snapshot(repo, component) != inputs:
        raise ValueError("source inputs changed during the build; rebuild the component")
    output = repo / OUTPUTS[component]
    extra = {}
    if component == "web":
        shell = output / "_shell.html"
        if not shell.is_file() or shell.is_symlink():
            raise ValueError("web output is missing a regular _shell.html")
        policy = content_security_policy(shell.read_text())
        headers = output / "_headers"
        if headers.is_symlink():
            raise ValueError("web output must not contain symlinks")
        headers.write_text(static_headers(policy))
        extra["content_security_policy"] = policy
    receipt = {"version": 1, "component": component, "inputs": inputs, **extra, "outputs": files(output)}
    (output / RECEIPT).write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n")
    state.unlink()


def check(repo, component):
    output = repo / OUTPUTS[component]
    try:
        receipt = json.loads((output / RECEIPT).read_text())
    except (OSError, ValueError) as error:
        raise ValueError(f"{component} build receipt is missing; run its build script") from error
    if receipt.get("version") != 1 or receipt.get("component") != component:
        raise ValueError("invalid build receipt")
    if component == "web":
        policy = receipt.get("content_security_policy")
        if not isinstance(policy, str) or not policy:
            raise ValueError("web build receipt has no content security policy")
        headers = repo / OUTPUTS[component] / "_headers"
        if headers.is_symlink() or not headers.is_file() or headers.read_text() != static_headers(policy):
            raise ValueError("web static headers do not match the build receipt")
    if receipt.get("inputs") != snapshot(repo, component):
        raise ValueError(f"{component} source inputs changed; run its build script")
    if receipt.get("outputs") != files(output):
        raise ValueError(f"{component} output changed or contains extra files; rebuild it")
    return receipt


def self_test():
    scratch = ROOT / ".artifacts/build-inputs-test"
    scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="fixture-", dir=scratch) as directory:
        repo = Path(directory)
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        (repo / ".gitignore").write_text(".artifacts/\napps/app/generated/\napps/app/dist/\n")
        (repo / "Cargo.toml").write_text("source one")
        for component in OUTPUTS:
            prepare(repo, component)
            output = repo / OUTPUTS[component]
            (output / ("_shell.html" if component == "web" else "asset")).write_text(
                "<script>fixture</script>" if component == "web" else component
            )
            finish(repo, component)
            check(repo, component)
        headers = (repo / OUTPUTS["web"] / "_headers").read_text()
        rules = []
        for line in headers.splitlines():
            if line and not line.startswith(" "):
                pattern = line
            elif line.startswith("  Cache-Control: "):
                rules.append((pattern, line.split(": ", 1)[1]))
        for path, expected in (
            ("/", "no-store"),
            ("/index.html", "no-store"),
            ("/vibeface_secure.js", "no-store"),
            ("/vibeface_secure_bg.wasm", "no-store"),
            ("/sw.js", "no-cache"),
            ("/assets/chunk-123.js", "public, max-age=31536000, immutable"),
        ):
            actual = [cache for pattern, cache in rules if fnmatch.fnmatchcase(path, pattern)]
            if actual != [expected]:
                raise AssertionError(f"{path}: expected one cache policy, got {actual}")
        public = repo / "apps/app/public"
        public.mkdir(parents=True)
        (public / ".env").write_text("PRIVATE_DATA=must-not-ship")
        try:
            sources(repo, "web")
        except ValueError:
            pass
        else:
            raise AssertionError("untracked public asset was accepted")
        (public / ".env").unlink()
        receipt = repo / OUTPUTS["crypto"] / RECEIPT
        saved_receipt = receipt.read_bytes()
        receipt.unlink()
        try:
            check(repo, "crypto")
        except ValueError:
            pass
        else:
            raise AssertionError("missing receipt was accepted")
        receipt.write_bytes(saved_receipt)
        output = repo / OUTPUTS["web"]
        (output / "private-note").write_text("must not ship")
        try:
            check(repo, "web")
        except ValueError:
            pass
        else:
            raise AssertionError("extra output was accepted")
        (output / "private-note").unlink()
        (repo / "Cargo.toml").write_text("changed source")
        for component in OUTPUTS:
            try:
                check(repo, component)
            except ValueError:
                pass
            else:
                raise AssertionError("stale source was accepted")
        prepare(repo, "crypto")
        assert not (repo / OUTPUTS["crypto"] / "asset").exists()
        (repo / "Cargo.toml").write_text("changed while building")
        try:
            finish(repo, "crypto")
        except ValueError:
            pass
        else:
            raise AssertionError("mid-build source change was accepted")
    print("Build input self-test passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("prepare", "finish", "check", "self-test"))
    parser.add_argument("component", choices=tuple(OUTPUTS), nargs="?")
    parser.add_argument("--cargo", action="store_true")
    args = parser.parse_args()
    if args.action == "self-test":
        self_test()
        return
    if not args.component:
        parser.error("component is required")
    if args.action == "check":
        receipt = check(ROOT, args.component)
        if args.cargo:
            for component in dict.fromkeys(("crypto", args.component)):
                component_receipt = check(ROOT, component)
                for name in component_receipt["inputs"]["source"]:
                    print(f"cargo:rerun-if-changed={ROOT / name}")
                for name in INPUTS[component]:
                    print(f"cargo:rerun-if-changed={ROOT / name}")
                print(f"cargo:rerun-if-changed={ROOT / OUTPUTS[component]}")
                print(f"cargo:rerun-if-changed={ROOT / OUTPUTS[component] / RECEIPT}")
    elif args.action == "prepare":
        prepare(ROOT, args.component)
    else:
        finish(ROOT, args.component)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"Build input verification failed: {error}") from error
