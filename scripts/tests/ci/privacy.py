"""Release artifact privacy and gatekeeper cases."""

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

import release_archive
import release_context
import release_format



def gatekeeper_test(root, scratch):
    import sys
    import textwrap

    workflow = (root / ".github/workflows/ci.yml").read_text()
    step = workflow.split("      - name: Verify Developer ID signature\n", 1)[1]
    script = textwrap.dedent(step.split("        run: |\n", 1)[1].split("\n      - ", 1)[0])
    accepted = (0, "Notarized Developer ID")
    pending = (3, "Unnotarized Developer ID")
    cases = (
        ("accepted", [accepted], False, 0),
        ("pending-accepted", [pending, accepted], False, 0),
        ("pending-exhausted", [pending] * 10, False, 3),
        ("other-status", [(2, pending[1])], False, 2),
        ("other-source", [(3, "fixture rejection")], False, 3),
        ("wrong-success-source", [(0, pending[1])], False, 1),
        ("binary-changed", [accepted], True, 1),
    )
    with tempfile.TemporaryDirectory(prefix="gatekeeper-", dir=scratch) as directory:
        fixture = Path(directory)
        tools = fixture / "bin"
        tools.mkdir()
        stub = tools / "stub"
        stub.write_text(f"#!{sys.executable}\n" + textwrap.dedent('''\
            import json, os, sys
            from pathlib import Path
            tool = Path(sys.argv[0]).name
            if tool == "codesign":
                binary = Path(sys.argv[-1])
                assert binary.parent == Path(os.environ["PACKAGE"])
                if sys.argv[1:] == ["--verify", "--strict", str(binary)]:
                    sys.exit(0)
                assert sys.argv[1:] == ["-dvvv", str(binary)]
                print("Identifier=cn.leaperone.vibeface\\nTeamIdentifier=FIXTURETEAM\\nCodeDirectory flags=0x10000(runtime)")
            elif tool == "spctl":
                binary = Path(sys.argv[-1])
                assert binary.parent == Path(os.environ["PACKAGE"])
                assert sys.argv[1:] == ["-a", "-vvv", "--ignore-cache", "--no-cache", "-t", "open", "--context", "context:primary-signature", str(binary)]
                state = Path(os.environ["RUNNER_TEMP"]) / ("calls-" + binary.name)
                index = int(state.read_text()) if state.exists() else 0
                state.write_text(str(index + 1))
                status, source = json.loads(os.environ["FIXTURE_ASSESSMENTS"])[index]
                if os.environ["FIXTURE_MUTATE"] == "true":
                    binary.write_text("changed")
                print("source=" + source)
                sys.exit(status)
            else:
                assert tool == "sleep" and len(sys.argv) == 2
                with open(Path(os.environ["RUNNER_TEMP"]) / "sleeps", "a") as sleeps:
                    sleeps.write(sys.argv[1] + "\\n")
            '''))
        stub.chmod(0o755)
        for tool in ("codesign", "spctl", "sleep"):
            (tools / tool).symlink_to(stub)
        for name, assessments, mutate, expected in cases:
            case = fixture / name
            case.mkdir()
            (case / "vibeface").write_text("original")
            (case / "vibeface-core").write_text("original")
            environment = dict(os.environ, PATH=f"{tools}{os.pathsep}{os.environ['PATH']}", PACKAGE=str(case), RUNNER_TEMP=str(case), VIBEFACE_CODESIGN_TEAM="FIXTURETEAM", FIXTURE_ASSESSMENTS=json.dumps(assessments), FIXTURE_MUTATE=json.dumps(mutate))
            result = subprocess.run(["bash", "--noprofile", "--norc", "-e", "-o", "pipefail"], input=script, env=environment, text=True, capture_output=True)
            assert result.returncode == expected, (name, result.returncode, result.stdout, result.stderr)
            completed_binaries = 2 if expected == 0 else 1
            for binary in (("vibeface", "vibeface-core") if completed_binaries == 2 else ("vibeface",)):
                assert int((case / ("calls-" + binary)).read_text()) == len(assessments), name
            sleeps = [int(line) for line in (case / "sleeps").read_text().split()] if (case / "sleeps").exists() else []
            assert len(sleeps) == completed_binaries * (len(assessments) - 1) and sleeps == sorted(sleeps), (name, sleeps)
            if name == "pending-exhausted":
                assert 600 * completed_binaries <= sum(sleeps) <= 900 * completed_binaries, ("waits long enough for Apple's ticket propagation", sleeps)
    print("Gatekeeper workflow control-flow self-tests passed; native assessment remains required")

def release_privacy_test(root, scratch):
    import shutil
    import tarfile

    with tempfile.TemporaryDirectory(prefix="release-", dir=scratch) as directory:
        repo = Path(directory)
        for name in ('scripts/release_component_identity.py', 'scripts/release_format.py', 'scripts/release_context.py', 'scripts/release_source.py', 'scripts/release_archive.py', 'scripts/release_inventory.py', 'scripts/release_github.py', 'scripts/release_publisher.py', 'scripts/release_assembly.py', "scripts/release.py", "scripts/offline-docs.py", "scripts/offline-docs.json", "scripts/fixtures/release-contract.json", "README.zh-CN.md", "apps/core/Cargo.toml", "apps/cli/Cargo.toml", "apps/app/package.json", "apps/mobile/app.json", "apps/desktop/Cargo.toml", "scripts/package-node.sh", "scripts/release-rustflags.py", "scripts/build-inputs.py", "scripts/install-node-service.sh", "Cargo.toml", "crates/control/src/lib.rs", "README.md"):
            target = repo / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(root / name, target)
        shutil.copyfile(root / "scripts/package-node-web.json", repo / "scripts/package-node-web.json")
        source = repo / "private build.rs"
        source.write_text('#[cfg(not(privacy_flag))] compile_error!("caller flag lost"); fn main() { println!("{}", file!()); }')
        binary = repo / "target/x86_64-unknown-linux-gnu/release/vibeface"
        binary.parent.mkdir(parents=True)
        environment = {**os.environ, "CARGO_TARGET_DIR": str(repo / "target"), "RUSTFLAGS": "--cfg privacy_flag", "PYTHONDONTWRITEBYTECODE": "1"}
        environment.pop("CARGO_ENCODED_RUSTFLAGS", None)
        for encoded in (None, "--cfg\x1fprivacy_flag", ""):
            if encoded is not None:
                environment["CARGO_ENCODED_RUSTFLAGS"] = encoded
                environment["RUSTFLAGS"] = "--cfg ignored_flag"
            flags = subprocess.check_output(["python3", str(repo / "scripts/release-rustflags.py")], env=environment, text=True).strip().split("\x1f")
            assert "ignored_flag" not in flags
            if encoded == "":
                assert "privacy_flag" not in flags
                continue
            subprocess.run(["rustc", "--crate-name", "privacy_fixture", "-Copt-level=3", "-Cdebuginfo=0", *flags, str(source), "-o", str(binary)], check=True)
            assert str(repo).encode() not in binary.read_bytes(), "release binary contains its source directory"
            assert subprocess.check_output([str(binary)], text=True).strip() == "/vibeface/private build.rs"
        (repo / "packages/docs").mkdir(parents=True)
        (repo / "packages/docs/guide.md").write_text("public guide")
        (repo / "packages/docs/node.md").write_text("node guide")
        (repo / "packages/docs/releasing.md").write_text("release guide")
        (repo / "README.md").write_text("runtime readme")
        for app, value in (("cli", "1.2.3-hourly.2026111400"), ("core", "2.3.4")):
            manifest = repo / f"apps/{app}/Cargo.toml"
            manifest.write_text(re.sub(r'^version = "[^"]+"$', f'version = "{value}"', manifest.read_text(), count=1, flags=re.M))
        app_manifest = repo / "apps/app/package.json"
        app_manifest.write_text(json.dumps(json.loads(app_manifest.read_text()) | {"version": "3.4.5-rc.2"}) + "\n")
        (repo / ".gitignore").write_text(".artifacts/\ntarget/\nbin/\nprivate build.rs\napps/app/generated/\napps/app/dist/\n.env\npackages/docs/untracked-note.md\ncargo-called\n")
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        subprocess.run(["git", "add", "."], cwd=repo, check=True)

        subprocess.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"], cwd=repo, check=True)

        def generated():
            tool = ["python3", str(repo / "scripts/build-inputs.py")]
            for component, files in (
                ("crypto", {"vibeface_secure.js": "fixture", "vibeface_secure_bg.wasm": "fixture"}),
                ("web", {"_shell.html": "fixture", "index.html": "fixture", "_redirects": "/* /_shell.html 200\n", "_headers": "/*\n  Content-Security-Policy: default-src 'self'\n", "vibeface_secure.js": "fixture", "vibeface_secure_bg.wasm": "wasm", ".vibeface-build.json": "{}"}),
            ):
                subprocess.run([*tool, "prepare", component], check=True)
                output = repo / ("apps/app/generated" if component == "crypto" else "apps/app/dist/client")
                for name, content in files.items():
                    (output / name).write_text(content)
                subprocess.run([*tool, "finish", component], check=True)

        generated()
        (repo / "packages/docs/.env").write_text("PRIVATE_FIXTURE=must-not-be-packaged")
        (repo / "packages/docs/untracked-note.md").write_text("must-not-be-packaged")
        if hasattr(os, "setxattr"):
            os.setxattr(repo / "packages/docs/guide.md", "user.build-host", b"private builder")
        environment.update({name: "" for name in ("VIBEFACE_CODESIGN_IDENTITY", "VIBEFACE_NOTARY_KEY", "VIBEFACE_NOTARY_KEY_ID", "VIBEFACE_NOTARY_ISSUER")})
        fake_cargo = repo / "bin/cargo"
        fake_cargo.parent.mkdir()
        fake_cargo.write_text('#!/bin/sh\nset -eu\ntouch "$CARGO_MARKER"\nmkdir -p "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release"\nprintf "#!/bin/sh\\nprintf \\\"vibeface 1.2.3-hourly.2026111400\\\\n\\\"\\n" > "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/vibeface"\nprintf "#!/bin/sh\\nprintf \\\"vibeface-core 2.3.4\\\\n\\\"\\n" > "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/vibeface-core"\nchmod +x "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/vibeface" "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/vibeface-core"\n')
        fake_cargo.chmod(0o755)
        marker = repo / "cargo-called"
        environment.update(PATH=f"{fake_cargo.parent}{os.pathsep}{environment['PATH']}", CARGO_MARKER=str(marker))
        subprocess.run(["bash", str(repo / "scripts/package-node.sh"), "--target", "x86_64-unknown-linux-gnu"], env=environment, check=True, stdout=subprocess.PIPE)
        assert marker.exists(), "packaging must always rebuild the native binary"
        archive_path, = (repo / ".artifacts/packages").glob("*.tar.gz")
        assert archive_path.read_bytes()[:8] == b"\x1f\x8b\x08\x00\x00\x00\x00\x00"
        with tarfile.open(archive_path) as archive:
            for entry in archive.getmembers():
                assert entry.uid == entry.gid == 0 and entry.uname == entry.gname == ""
                assert entry.mtime == 0 and not entry.pax_headers
                assert entry.isfile() or entry.isdir()
                assert entry.mode == (0o755 if entry.isdir() or entry.mode & 0o111 else 0o644)
                assert not any(part.startswith("._") for part in Path(entry.name).parts)
            package = archive.getmembers()[0].name
            assert archive.extractfile(f"{package}/docs/guide.md").read() == b"public guide"
            assert not any(entry.name.endswith(("/.env", "/untracked-note.md")) for entry in archive.getmembers())
            assert archive.getmember(f"{package}/vibeface").mode & 0o111
            assert archive.getmember(f"{package}/vibeface-core").mode & 0o111
            info = json.load(archive.extractfile(f"{package}/BUILD-INFO.json"))
            assert [info[key] for key in ("version", "core_version", "app_version")] == ["1.2.3-hourly.2026111400", "2.3.4", "3.4.5-rc.2"]
            assert archive.extractfile(f"{package}/RUNTIME-REQUIREMENTS.md").read() == release_archive.RUNTIME_REQUIREMENTS.encode("utf-8")
            unpacked = repo / ".artifacts/sealed-fixture"
            archive.extractall(unpacked, filter="data")
        assert release_archive.verify_bundle(unpacked / package, "x86_64-unknown-linux-gnu", {
            "version": "1.2.3-hourly.2026111400", "source_commit": info["source_commit"],
            "dependencies": {"core": {"version": "2.3.4", "source_commit": info["source_commit"]},
                             "app": {"version": "3.4.5-rc.2", "source_commit": info["source_commit"]}},
        }) == info
        from unittest.mock import patch
        with patch.object(release_context, 'ROOT', repo), patch.dict(os.environ, {"PACKAGE_TARGET": "x86_64-unknown-linux-gnu"}):
            sealed = release_archive.seal_bundle(unpacked / package,
                                         {"version": "2.3.4", "source_commit": "b" * 40},
                                         {"version": "3.4.5-rc.2", "source_commit": "c" * 40})
        assert json.loads((unpacked / package / "BUILD-INFO.json").read_text()) == {
            "version": "1.2.3-hourly.2026111400", "target": "x86_64-unknown-linux-gnu", "source_commit": info["source_commit"],
            "update_protocol": 1, "data_epoch": 1, "core_version": "2.3.4", "core_source_commit": "b" * 40,
            "app_version": "3.4.5-rc.2", "app_source_commit": "c" * 40,
        }
        release_format.verify_build_info(sealed, release_context.contracts(), release_context.runtime_definitions())
        (repo / "scripts/release-rustflags.py").write_text('raise RuntimeError("fixture failure")')
        generated()
        marker.unlink()
        assert subprocess.run(["bash", str(repo / "scripts/package-node.sh"), "--target", "x86_64-unknown-linux-gnu"], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE).returncode != 0
        assert not marker.exists(), "a failed privacy setup must stop the build"
    print("Release privacy self-test passed: independent nine-field bundle identities, caller flags, source paths, archive owners and extended attributes")
