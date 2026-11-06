#!/usr/bin/env python3
"""Verify source-bound Web/WASM, native and iOS build inputs."""
import argparse
import base64
import fnmatch
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import zipfile
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


def origin(repo):
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
    tree = subprocess.check_output(["git", "rev-parse", "HEAD^{tree}"], cwd=repo, text=True).strip()
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=repo):
        raise ValueError("reusable binaries require a clean source checkout")
    result = dict(source_commit=commit, source_tree=tree, repository="local", workflow="local", run_id=0, job="local", attempt=0)
    if os.environ.get("GITHUB_ACTIONS") == "true":
        repository = os.environ["GITHUB_REPOSITORY"]
        workflow = os.environ["GITHUB_WORKFLOW_REF"].split("@", 1)[0]
        if os.environ["GITHUB_SHA"] != commit or not workflow.startswith(repository + "/.github/workflows/"):
            raise ValueError("build source differs from the workflow identity")
        result.update(repository=repository, workflow=workflow[len(repository) + 1:],
                      run_id=int(os.environ["GITHUB_RUN_ID"]), job=os.environ["GITHUB_JOB"],
                      attempt=int(os.environ["GITHUB_RUN_ATTEMPT"]))
        if result["run_id"] <= 0 or result["attempt"] <= 0:
            raise ValueError("invalid Actions producer identity")
    return result


def verify_origin(repo, recorded, component, producer_attempt):
    expected = origin(repo)
    if expected["repository"] != "local":
        workflow = expected["workflow"]
        if workflow not in {".github/workflows/ci.yml", ".github/workflows/mobile.yml"}:
            raise ValueError("unsupported input workflow")
        expected["job"] = "ios-build" if workflow.endswith("/mobile.yml") else "build"
        if producer_attempt is None or not 1 <= producer_attempt <= expected["attempt"]:
            raise ValueError("missing or invalid producer attempt")
        expected["attempt"] = producer_attempt
        if component == "ios" and workflow != ".github/workflows/mobile.yml":
            raise ValueError("iOS inputs require the mobile workflow")
    if recorded != expected:
        raise ValueError("input source, workflow, run, job or producer attempt differs")


def input_files(directory):
    result = {}
    for path in sorted(directory.rglob("*")):
        metadata = path.lstat()
        if stat.S_ISDIR(metadata.st_mode):
            continue
        if not stat.S_ISREG(metadata.st_mode):
            raise ValueError("build inputs must be regular files")
        name = path.relative_to(directory).as_posix()
        if name != "manifest.json":
            result[name] = dict(sha256=digest(path.read_bytes()), size=metadata.st_size, mode=stat.S_IMODE(metadata.st_mode))
    return result


def write_input(directory, archive, component, target, source):
    manifest = dict(version=1, component=component, target=target, origin=source, files=input_files(directory))
    (directory / "manifest.json").write_text(json.dumps(manifest, sort_keys=True) + "\n")
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "w:gz", format=tarfile.USTAR_FORMAT) as output:
        for path in sorted(directory.rglob("*")):
            if path.is_file():
                info = output.gettarinfo(str(path), arcname=path.relative_to(directory).as_posix())
                info.uid = info.gid = info.mtime = 0
                info.uname = info.gname = ""
                with path.open("rb") as data:
                    output.addfile(info, data)


def check_input(repo, directory, component, target, producer_attempt):
    manifest_path = directory / "manifest.json"
    if manifest_path.is_symlink() or not manifest_path.is_file():
        raise ValueError("missing regular input manifest")
    manifest = json.loads(manifest_path.read_text())
    if (set(manifest) != {"version", "component", "target", "origin", "files"}
            or manifest["version"] != 1 or manifest["component"] != component or manifest["target"] != target):
        raise ValueError("input kind or target differs")
    verify_origin(repo, manifest["origin"], component, producer_attempt)
    actual = input_files(directory)
    if actual != manifest["files"]:
        raise ValueError("input files, modes or digests differ")
    native = {"vibeface", "vibeface-core"}
    if component == "native":
        if set(actual) != native:
            raise ValueError("native input requires exactly CLI and Core")
    else:
        required = {"app.zip", "host/vibeface", "host/vibeface-core", "host/manifest.json", "crypto/" + RECEIPT, "web/" + RECEIPT}
        if not required <= set(actual) or any(name not in required and not name.startswith(("crypto/", "web/")) for name in actual):
            raise ValueError("iOS input contains missing or private files")
        check_input(repo, directory / "host", "native", "aarch64-apple-darwin", producer_attempt)
        for component_name in OUTPUTS:
            name = "crypto" if component_name == "crypto" else "web"
            receipt = json.loads((directory / name / RECEIPT).read_text())
            if receipt.get("outputs") != files(directory / name):
                raise ValueError("iOS generated files differ from their receipt")
        for name in ("vibeface_secure.js", "vibeface_secure_bg.wasm"):
            if (directory / "crypto" / name).read_bytes() != (directory / "web" / name).read_bytes():
                raise ValueError("iOS Web and crypto bytes differ")
        native = {"host/" + name for name in native}
    if any(actual[name]["size"] <= 0 or actual[name]["mode"] != 0o755 for name in native):
        raise ValueError("native binaries must be nonempty and executable")
    return manifest


def restore_input(repo, archive, output, component, target, producer_attempt):
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists() or output.is_symlink():
        raise ValueError("input destination must be new")
    with tempfile.TemporaryDirectory(prefix=".restore-", dir=output.parent) as temporary:
        directory = Path(temporary)
        with tarfile.open(archive, "r:gz") as packed:
            members = packed.getmembers()
            names = [member.name for member in members]
            if len(members) > 16384 or sum(member.size for member in members) > 2 * 1024 ** 3 or len(names) != len(set(names)):
                raise ValueError("input archive exceeds limits or has duplicate members")
            for member in members:
                parts = member.name.split("/")
                if (not member.isfile() or any(part in {"", ".", ".."} for part in parts)
                        or "\\" in member.name or ":" in member.name or member.mode not in {0o644, 0o755}):
                    raise ValueError("unsafe input archive member")
            for member in members:
                path = directory / member.name
                path.parent.mkdir(parents=True, exist_ok=True)
                with packed.extractfile(member) as source, path.open("xb") as destination:
                    shutil.copyfileobj(source, destination)
                path.chmod(member.mode)
        check_input(repo, directory, component, target, producer_attempt)
        directory.rename(output)


def seal_native(repo, source, target, output):
    source_identity = origin(repo)
    with tempfile.TemporaryDirectory(prefix=".native-", dir=output.parent) as temporary:
        directory = Path(temporary)
        for name in ("vibeface", "vibeface-core"):
            path = source / name
            if path.is_symlink() or not path.is_file() or not os.access(path, os.X_OK):
                raise ValueError("native binary is missing or not executable")
            shutil.copyfile(path, directory / name)
            (directory / name).chmod(0o755)
        write_input(directory, output, "native", target, source_identity)


def pack_ios(repo, app, native, output):
    check_app_zip(app)
    check(repo, "crypto")
    check(repo, "web")
    with tempfile.TemporaryDirectory(prefix=".ios-", dir=output.parent) as temporary:
        directory = Path(temporary)
        restore_input(repo, native, directory / "host", "native", "aarch64-apple-darwin", int(os.environ.get("GITHUB_RUN_ATTEMPT", "0")))
        shutil.copyfile(app, directory / "app.zip")
        (directory / "app.zip").chmod(0o644)
        for component, path in OUTPUTS.items():
            shutil.copytree(repo / path, directory / component)
            for copied in (directory / component).rglob("*"):
                copied.chmod(0o755 if copied.is_dir() else 0o644)
        write_input(directory, output, "ios", "aarch64-apple-ios-sim", origin(repo))


def check_app_zip(path):
    if path.is_symlink() or not path.is_file():
        raise ValueError("simulator app must be a regular ZIP")
    with zipfile.ZipFile(path) as archive:
        members = archive.infolist()
        names = [member.filename.rstrip("/") for member in members]
        if len(names) != len(set(names)) or len(names) > 16384 or sum(member.file_size for member in members) > 1024 ** 3:
            raise ValueError("invalid app ZIP size or duplicate members")
        for member, name in zip(members, names):
            parts = name.split("/")
            mode = member.external_attr >> 16
            if (parts[0] != "Vibeface.app" or any(part in {"", ".", ".."} for part in parts)
                    or "\\" in name or ":" in name or stat.S_IFMT(mode) not in {0, stat.S_IFDIR, stat.S_IFREG}):
                raise ValueError("unsafe simulator app ZIP member")


def install_ios(repo, source, producer_attempt):
    check_input(repo, source, "ios", "aarch64-apple-ios-sim", producer_attempt)
    app_dir = repo / ".artifacts/native/ios-build/Build/Products/Release-iphonesimulator"
    app_dir.mkdir(parents=True, exist_ok=True)
    check_app_zip(source / "app.zip")
    subprocess.run(["unzip", "-q", str(source / "app.zip"), "-d", str(app_dir)], check=True)
    for component, path in OUTPUTS.items():
        destination = repo / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(source / component, destination)
        check(repo, component)
    host = repo / ".artifacts/native/target/release"
    host.mkdir(parents=True, exist_ok=True)
    for name in ("vibeface", "vibeface-core"):
        shutil.copy2(source / "host" / name, host / name)


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
    input_self_test(scratch)
    print("Build input self-test passed")


def input_self_test(scratch):
    from unittest.mock import patch
    with tempfile.TemporaryDirectory(prefix="inputs-", dir=scratch) as temporary:
        repo = Path(temporary)
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        (repo / ".gitignore").write_text(".artifacts/\napps/app/generated/\napps/app/dist/\n")
        subprocess.run(["git", "add", "."], cwd=repo, check=True)
        subprocess.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"], cwd=repo, check=True)
        build = repo / ".artifacts/build"
        build.mkdir(parents=True)
        for name in ("vibeface", "vibeface-core"):
            (build / name).write_bytes(b"fixture binary")
            (build / name).chmod(0o755)
        env = dict(GITHUB_ACTIONS="true", GITHUB_REPOSITORY="fixture/repo", GITHUB_WORKFLOW_REF="fixture/repo/.github/workflows/ci.yml@refs/heads/fixture",
                   GITHUB_RUN_ID="42", GITHUB_RUN_ATTEMPT="1", GITHUB_JOB="build",
                   GITHUB_SHA=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip())
        archive = build.parent / "native.tar.gz"
        target = "aarch64-apple-darwin"
        with patch.dict(os.environ, env):
            seal_native(repo, build, target, archive)
            os.environ["GITHUB_JOB"] = "assemble"
            os.environ["GITHUB_RUN_ATTEMPT"] = "2"
            restored = build.parent / "restored"
            restore_input(repo, archive, restored, "native", target, 1)
            assert (restored / "vibeface").read_bytes() == b"fixture binary"
            assert (restored / "vibeface").stat().st_mode & 0o777 == 0o755

            def rejected(action):
                try:
                    action()
                except (ValueError, OSError):
                    return
                raise AssertionError("unsafe or mismatched build input was accepted")

            for attempt in (None, 0, 2, 3):
                rejected(lambda: check_input(repo, restored, "native", target, attempt))
            rejected(lambda: check_input(repo, restored, "native", "x86_64-unknown-linux-gnu", 1))
            manifest = restored / "manifest.json"
            original = manifest.read_text()
            for key, value in (("source_commit", "0" * 40), ("source_tree", "0" * 40), ("workflow", ".github/workflows/release.yml"), ("repository", "other/repo"), ("run_id", 43), ("job", "wrong")):
                data = json.loads(original)
                data["origin"][key] = value
                manifest.write_text(json.dumps(data))
                rejected(lambda: check_input(repo, restored, "native", target, 1))
            manifest.write_text(original)
            binary = restored / "vibeface"
            for mutation in (lambda: binary.write_bytes(b"changed"), lambda: binary.chmod(0o644), lambda: binary.unlink(), lambda: (restored / "private").write_text("private")):
                mutation()
                rejected(lambda: check_input(repo, restored, "native", target, 1))
                binary.write_bytes(b"fixture binary")
                binary.chmod(0o755)
                (restored / "private").unlink(missing_ok=True)
            import io
            for name, kind in (("../escape", tarfile.REGTYPE), ("/absolute", tarfile.REGTYPE), ("link", tarfile.SYMTYPE), ("hard", tarfile.LNKTYPE), ("device", tarfile.CHRTYPE), ("duplicate", tarfile.REGTYPE)):
                bad = build.parent / "bad.tar.gz"
                with tarfile.open(bad, "w:gz") as packed:
                    member = tarfile.TarInfo(name)
                    member.type, member.mode, member.size = kind, 0o644, 1 if kind == tarfile.REGTYPE else 0
                    packed.addfile(member, io.BytesIO(b"x") if member.size else None)
                    if name == "duplicate":
                        packed.addfile(member, io.BytesIO(b"x"))
                rejected(lambda: restore_input(repo, bad, build.parent / "bad-output", "native", target, 1))
                assert not (build.parent / "bad-output").exists()
            os.environ.update(GITHUB_WORKFLOW_REF="fixture/repo/.github/workflows/mobile.yml@refs/heads/fixture", GITHUB_JOB="ios-build", GITHUB_RUN_ATTEMPT="1")
            seal_native(repo, build, target, archive)
            for component in OUTPUTS:
                prepare(repo, component)
                directory = repo / OUTPUTS[component]
                for name in ("vibeface_secure.js", "vibeface_secure_bg.wasm"):
                    (directory / name).write_bytes(b"public fixture")
                if component == "web":
                    (directory / "_shell.html").write_text("<script>fixture</script>")
                finish(repo, component)
            app = build.parent / "app.zip"
            with zipfile.ZipFile(app, "w") as packed:
                packed.writestr("Vibeface.app/Vibeface", b"public app fixture")
            ios = build.parent / "ios.tar.gz"
            pack_ios(repo, app, archive, ios)
            os.environ.update(GITHUB_JOB="ios-accept", GITHUB_RUN_ATTEMPT="2")
            restored_ios = build.parent / "ios-restored"
            restore_input(repo, ios, restored_ios, "ios", "aarch64-apple-ios-sim", 1)
            for path in OUTPUTS.values():
                shutil.rmtree(repo / path)
            install_ios(repo, restored_ios, 1)
            assert (repo / ".artifacts/native/target/release/vibeface").stat().st_mode & 0o777 == 0o755
            assert (repo / ".artifacts/native/ios-build/Build/Products/Release-iphonesimulator/Vibeface.app/Vibeface").read_bytes() == b"public app fixture"
            (restored_ios / "private").mkdir()
            (restored_ios / "private/invitation.txt").write_text("must not enter inputs")
            rejected(lambda: check_input(repo, restored_ios, "ios", "aarch64-apple-ios-sim", 1))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("prepare", "finish", "check", "self-test", "seal", "restore"))
    parser.add_argument("component", choices=(*OUTPUTS, "native", "ios"), nargs="?")
    parser.add_argument("--cargo", action="store_true")
    parser.add_argument("--input", type=Path)
    parser.add_argument("--source", type=Path)
    parser.add_argument("--app", type=Path)
    parser.add_argument("--native", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--producer-attempt", type=int)
    args = parser.parse_args()
    if args.action == "self-test":
        self_test()
        return
    if not args.component:
        parser.error("component is required")
    if args.component in {"native", "ios"}:
        target = args.target if args.component == "native" else "aarch64-apple-ios-sim"
        if not target or not re.fullmatch(r"[a-z0-9_-]+", target):
            parser.error("a valid target is required")
        if args.action == "check" and args.input:
            check_input(ROOT, args.input, args.component, target, args.producer_attempt)
        elif args.action == "restore" and args.input and args.output:
            restore_input(ROOT, args.input, args.output, args.component, target, args.producer_attempt)
            if args.component == "ios":
                install_ios(ROOT, args.output, args.producer_attempt)
        elif args.action == "seal" and args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            if args.component == "native" and args.source:
                seal_native(ROOT, args.source, target, args.output)
            elif args.component == "ios" and args.app and args.native:
                pack_ios(ROOT, args.app, args.native, args.output)
            else:
                parser.error("seal needs native source or app and native archives")
        else:
            parser.error("input action needs its source and destination")
        return
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
