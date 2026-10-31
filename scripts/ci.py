#!/usr/bin/env python3
"""Select CI coverage once, run its Rust test tiers, and check the final gate."""

import argparse
import datetime
import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


PACKAGES = ("vibeface-protocol", "vibeface-bridge", "vibeface-secure", "vibeface", "vibeface-secure-ffi", "vibeface-delivery")
DEPENDENTS = {
    "protocol": ("bridge", "node"),
    "bridge": ("node",),
    "secure": ("node", "secure-ffi"),
    "node": (),
    "secure-ffi": (),
    "delivery": ("node",),
}
CRATES = dict(zip(("protocol", "bridge", "secure", "node", "secure-ffi", "delivery"), PACKAGES))
PATH_RULES = (
    ("delivery", ("crates/delivery/*", "proofs/delivery/*", "contracts/delivery/*", "tests/fixtures/delivery-probe/*", "crates/node/src/delivery/*", "crates/node/src/client.rs", "crates/node/src/config.rs", "crates/node/src/main.rs", "crates/node/src/maintenance.rs", "crates/node/src/net.rs", "crates/node/src/net/pkarr.rs", "crates/node/src/runtime.rs", "crates/node/src/runtime/peers.rs", "scripts/check-delivery.py", "scripts/install-delivery-toolchain.py", "scripts/run-delivery-live.py")),
    ("full", ("Cargo.toml", "Cargo.lock", "crates/*/Cargo.toml", "rust-toolchain*", ".cargo/*", ".github/*", ".preflight.toml", "scripts/ci.py")),
    ("www", ("apps/www/*",)),
    ("docs", ("README.md", "AGENTS.md", "CLAUDE.md", "docs/*")),
    ("node", ("packages/client/src/avatar/derived-faces.json", "packages/client/src/avatar/face.ts")),
    ("web", ("crates/node/web/*", "packages/client/*", "packages/theme/*", "packages/secure-native/*", "apps/mobile/*", "package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml", "scripts/build-web.sh", "scripts/build-web-crypto.sh")),
    ("protocol", ("crates/protocol/*",)),
    ("bridge", ("crates/bridge/*",)),
    ("secure", ("crates/secure/*", "scripts/crypto-client.mjs")),
    ("secure-ffi", ("crates/secure-ffi/*", "scripts/build-mobile-secure.sh")),
    ("node", ("crates/node/*", "install.sh", "scripts/package-node.sh", "scripts/install-node-service.sh", "scripts/verify-node.py")),
)
WWW_PATHS = ("apps/www/*", "docs/*", "packages/theme/*", "package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml")
SLOW_TESTS = (
    "store::tests::append_100k_stays_flat",
    "runtime::peers::tests::a_round_left_behind_pulls_again_without_waiting_for_hello",
)
PLATFORMS = (
    {"runner": "ubuntu-24.04", "target": "x86_64-unknown-linux-gnu"},
    {"runner": "ubuntu-24.04-arm", "target": "aarch64-unknown-linux-gnu"},
    {"runner": "macos-15", "target": "aarch64-apple-darwin"},
)
JOBS = ("quick-check", "crypto-web", "web", "rust-check", "rust-fast", "rust-slow", "delivery", "package", "npm-package", "npm-smoke", "www")
NIGHTLY_LABEL = "nightly"


def make_plan(paths, event="pull_request", ref_type="branch", channel="nightly", nightly=False):
    scopes = set()
    for path in paths:
        scopes.add(next((scope for scope, patterns in PATH_RULES if any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns)), "full"))
        if any(fnmatch.fnmatchcase(path, pattern) for pattern in WWW_PATHS):
            scopes.add("www")
    return dict(scope_plan(scopes, event, ref_type, channel, nightly=nightly), changed_files=len(paths))


def scope_plan(scopes, event, ref_type, channel, nightly=False):
    if (
        scopes - {"full", "docs", "web", "www", *DEPENDENTS}
        or event not in {"pull_request", "push", "schedule", "workflow_dispatch", "merge_group"}
        or ref_type not in {"branch", "tag"}
        or channel not in {"stable", "rc", "nightly"}
        or (nightly and event != "pull_request")
    ):
        raise ValueError("Invalid CI scopes or scenario")
    extended = event in {"schedule", "workflow_dispatch"} or ref_type == "tag"
    full = "full" in scopes or extended or event == "merge_group"
    affected = set(DEPENDENTS) if full else scopes & set(DEPENDENTS)
    for crate in tuple(affected):
        pending = list(DEPENDENTS[crate])
        while pending:
            dependent = pending.pop()
            if dependent not in affected:
                affected.add(dependent)
                pending.extend(DEPENDENTS[dependent])
    packages = [package for crate, package in CRATES.items() if crate in affected]
    web = full or "web" in scopes or "node" in affected
    www = full or "www" in scopes
    slow = "node" in affected and extended
    delivery = full or "delivery" in affected
    platforms = PLATFORMS if "node" in affected and extended else PLATFORMS[:1]
    plan = {
        "full": full,
        "extended": extended,
        "check_msrv": bool(packages) and extended,
        "docs_only": not web and not packages,
        "event": event,
        "ref_type": ref_type,
        "channel": channel,
        "scopes": sorted(scopes),
        "web": web,
        "www": www,
        "node": "node" in affected,
        "delivery": delivery,
        "rust_packages": packages,
        "test_tiers": (["fast"] if packages else []) + (["slow"] if slow else []),
        "package_mode": "node" if "node" in affected and extended else "web",
        "package_matrix": {"include": list(platforms) if web else []},
        "npm": channel == "nightly" and "node" in affected and extended,
        "nightly": nightly,
    }
    if nightly:
        plan.update(
            full=False,
            extended=False,
            check_msrv=False,
            docs_only=False,
            web=False,
            www=False,
            node=False,
            delivery=False,
            rust_packages=[],
            test_tiers=[],
            package_mode="web",
            package_matrix={"include": []},
            npm=False,
            required_jobs=[],
        )
        return plan
    required = {"quick-check"}
    if web:
        required.update(("crypto-web", "web", "package"))
    if packages:
        required.update(("rust-check", "rust-fast"))
    if slow:
        required.add("rust-slow")
    if delivery:
        required.add("delivery")
    if plan["npm"]:
        required.update(("npm-package", "npm-smoke"))
    if www:
        required.add("www")
    plan["required_jobs"] = [job for job in JOBS if job in required]
    return plan


def git(*args, cwd=None):
    return subprocess.check_output(["git", *args], cwd=cwd)


def changed_paths(base, head, event, cwd=None):
    base = git("rev-parse", "--verify", "--end-of-options", f"{base}^{{commit}}", cwd=cwd).decode().strip()
    head = git("rev-parse", "--verify", "--end-of-options", f"{head}^{{commit}}", cwd=cwd).decode().strip()
    if event == "pull_request":
        base = git("merge-base", base, head, cwd=cwd).decode().strip()
    # Disabling rename folding includes both old and new paths, including cross-scope moves.
    return [os.fsdecode(path) for path in git("diff", "--name-only", "--no-renames", "-z", base, head, cwd=cwd).split(b"\0") if path]


def event_plan(args):
    event = args.event or os.environ.get("GITHUB_EVENT_NAME", "pull_request")
    ref_type = os.environ.get("GITHUB_REF_TYPE", "branch")
    payload_path = os.environ.get("GITHUB_EVENT_PATH")
    payload = json.loads(Path(payload_path).read_text()) if payload_path else {}
    base, head = args.base, args.head
    if not base and event == "pull_request":
        pr = payload["pull_request"]
        base, head = pr["base"]["sha"], pr["head"]["sha"]
    elif not base and event == "push":
        base, head = payload.get("before"), payload.get("after")
    forced = event in {"schedule", "workflow_dispatch", "merge_group"} or ref_type == "tag"
    if forced:
        paths = []
    elif not base or not head or set(base) == {"0"}:
        paths = ["__unknown_build_inputs__"]
    else:
        paths = changed_paths(base, head, event)
    metadata = release_metadata(ref_type, os.environ.get("GITHUB_REF_NAME", ""))
    nightly = event == "pull_request" and any(
        label.get("name") == NIGHTLY_LABEL for label in payload.get("pull_request", {}).get("labels", [])
    )
    plan = make_plan(paths, event, ref_type, metadata["channel"], nightly=nightly)
    plan.update(metadata)
    return plan


def release_metadata(ref_type, ref_name):
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"], text=True))
    package = next(package for package in metadata["packages"] if package["name"] == "vibeface")
    version = package["version"]
    number = r"(?:0|[1-9][0-9]*)"
    match = re.fullmatch(rf"{number}\.{number}\.{number}(?:-(nightly)\.([0-9]{{8}})\.([1-9][0-9]*)|-(rc)\.([1-9][0-9]*))?", version)
    if not match:
        raise ValueError("Expected X.Y.Z, X.Y.Z-nightly.YYYYMMDD.N or X.Y.Z-rc.N")
    channel = "nightly" if match[1] else "rc" if match[4] else "stable"
    if channel == "nightly":
        date = match[2]
        datetime.date(int(date[:4]), int(date[4:6]), int(date[6:]))
    if ref_type == "tag":
        if ref_name != f"v{version}":
            raise ValueError("Release tag must exactly match the Cargo version")
        git("merge-base", "--is-ancestor", "HEAD", "origin/main")
    return {"version": version, "msrv": package["rust_version"], "channel": channel, "prerelease": channel != "stable"}


def write_plan(plan, output=None):
    encoded = json.dumps(plan, separators=(",", ":"))
    print(encoded)
    if output:
        outputs = {
            "plan": encoded,
            "web": str(plan["web"]).lower(),
            "www": str(plan["www"]).lower(),
            "rust": str(bool(plan["rust_packages"])).lower(),
            "slow": str("slow" in plan["test_tiers"]).lower(),
            "delivery": str(plan["delivery"]).lower(),
            "package": str("package" in plan["required_jobs"]).lower(),
            "package_matrix": json.dumps(plan["package_matrix"], separators=(",", ":")),
            "package_mode": plan["package_mode"],
            "npm": str(plan["npm"]).lower(),
            "version": plan["version"],
            "msrv": plan["msrv"],
            "channel": plan["channel"],
            "prerelease": str(plan["prerelease"]).lower(),
            "nightly": str(plan["nightly"]).lower(),
        }
        with open(output, "a") as destination:
            destination.writelines(f"{key}={value}\n" for key, value in outputs.items())
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as destination:
            destination.write(
                "### Selected CI coverage\n\n"
                f"- Changed paths: {plan['changed_files']}\n"
                f"- Full workspace scope: {plan['full']}\n"
                f"- Nightly bypass: {plan['nightly']}\n"
                f"- Coverage tier: {'extended' if plan['extended'] else 'core'}\n"
                f"- Minimum Rust version check: {plan['check_msrv']}\n"
                f"- Rust packages: {', '.join(plan['rust_packages']) or 'none'}\n"
                f"- Test tiers: {', '.join(plan['test_tiers']) or 'none'}\n"
                f"- Package acceptance: {plan['package_mode'] if plan['web'] else 'none'}\n"
                f"- Required jobs: {', '.join(plan['required_jobs'])}\n"
            )


def gate(needs, plan=None, delivery_live=False):
    if needs.get("plan", {}).get("result") != "success":
        raise ValueError("CI plan did not succeed")
    if plan is None:
        plan = json.loads(needs["plan"]["outputs"]["plan"])
    expected = scope_plan(
        set(plan["scopes"]), plan["event"], plan["ref_type"], plan["channel"], nightly=plan.get("nightly", False)
    )
    if any(plan.get(key) != value for key, value in expected.items()):
        raise ValueError("Required coverage differs from the selected CI scope and scenario")
    if delivery_live and plan["event"] != "workflow_dispatch":
        raise ValueError("Live delivery proofs require explicit workflow_dispatch")
    required = expected["required_jobs"] + (["delivery-live"] if delivery_live else [])
    failed = [job for job in required if needs.get(job, {}).get("result") != "success"]
    failed.extend(job for job in (*JOBS, "delivery-live") if job not in required and needs.get(job, {}).get("result") in {"failure", "cancelled"})
    if failed:
        raise ValueError(f"Required CI checks did not succeed: {', '.join(failed)}")
    print(f"CI gate passed: {', '.join(required)}")


def package_args(packages):
    if not packages or len(set(packages)) != len(packages) or set(packages) - set(PACKAGES):
        raise ValueError("Expected unique workspace package names")
    return [argument for package in packages for argument in ("-p", package)]


def validate_slow_tests(names):
    for slow in SLOW_TESTS:
        matches = [name for name in names if slow in name]
        if matches != [slow]:
            raise ValueError(f"Slow-test filter must match exactly one existing test: {slow}")
    fast = set(names) - set(SLOW_TESTS)
    if not fast:
        raise ValueError("Invalid fast/slow test partition")
    return fast


def run_test(packages, tier):
    selected = package_args(packages)
    if "vibeface" in packages:
        listing_args = selected if tier == "fast" else ["-p", "vibeface", "--lib"]
        listing = subprocess.check_output(["cargo", "test", "--locked", *listing_args, "--", "--list"], text=True)
        names = [line[:-6] for line in listing.splitlines() if line.endswith(": test")]
        fast = validate_slow_tests(names)
        print(f"Partition: {len(fast)} fast test names, {len(SLOW_TESTS)} exact slow tests", flush=True)
    if tier == "slow":
        if "vibeface" not in packages:
            raise ValueError("Slow tier requires the Node crate")
        command = ["cargo", "test", "--locked", "-p", "vibeface", "--lib", "--", "--exact", "--include-ignored", *SLOW_TESTS]
    else:
        skips = [argument for name in SLOW_TESTS for argument in ("--skip", name)]
        command = ["cargo", "test", "--locked", *selected, "--", *skips]
    subprocess.run(command, check=True)


def run_check(packages, msrv=None):
    selected = package_args(packages)
    subprocess.run(["cargo", "clippy", "--all-targets", "--locked", *selected, "--", "-D", "warnings"], check=True)
    if msrv:
        subprocess.run(["rustup", "toolchain", "install", msrv, "--profile", "minimal"], check=True)
        subprocess.run(["cargo", f"+{msrv}", "check", "--locked", *selected], check=True)


def self_test():
    from unittest.mock import patch
    web = make_plan(["crates/node/web/src/routes/index.tsx"])
    assert web["rust_packages"] == [] and web["test_tiers"] == []
    assert web["required_jobs"] == ["quick-check", "crypto-web", "web", "package"]
    assert web["package_mode"] == "web" and web["package_matrix"]["include"] == list(PLATFORMS[:1])
    assert make_plan(["crates/node/web/src/content.md"])["web"]
    www = make_plan(["apps/www/src/routes/index.tsx"])
    assert www["www"] and not www["web"] and not www["rust_packages"]
    assert www["required_jobs"] == ["quick-check", "www"]
    assert make_plan(["packages/client/src/lib/transport.ts", "pnpm-lock.yaml"])["scopes"] == ["web", "www"]
    assert make_plan(["apps/mobile/app/_layout.tsx", "packages/theme/theme.css"])["scopes"] == ["web", "www"]
    assert make_plan(["crates/secure-ffi/src/lib.rs"])["rust_packages"] == ["vibeface-secure-ffi"]
    nightly = make_plan(["crates/node/src/api.rs"], nightly=True)
    assert nightly["nightly"] and nightly["required_jobs"] == []
    assert nightly["rust_packages"] == [] and nightly["package_matrix"]["include"] == []
    for event in ("push", "schedule", "workflow_dispatch", "merge_group"):
        expect_failure(lambda event=event: scope_plan(set(), event, "branch", "nightly", nightly=True))
    docs = make_plan(["README.md", "docs/node.md"])
    assert docs["docs_only"] and docs["www"] and docs["required_jobs"] == ["quick-check", "www"]
    expected = {
        "node": ["vibeface"],
        "bridge": ["vibeface-bridge", "vibeface"],
        "protocol": ["vibeface-protocol", "vibeface-bridge", "vibeface"],
        "secure": ["vibeface-secure", "vibeface", "vibeface-secure-ffi"],
        "delivery": ["vibeface", "vibeface-delivery"],
    }
    for crate, packages in expected.items():
        plan = make_plan([f"crates/{crate}/src/lib.rs"])
        assert plan["rust_packages"] == packages and plan["test_tiers"] == ["fast"]
        assert plan["package_mode"] == "web" and plan["package_matrix"]["include"] == list(PLATFORMS[:1])
    for path in ("crates/delivery/Cargo.toml", "crates/delivery/abi/Statement.sol", "proofs/delivery/guest/src/main.rs", "proofs/delivery/fixtures/manifest.json", "contracts/delivery/DeliveryEscrow.sol", "contracts/delivery/package-lock.json", "tests/fixtures/delivery-probe/Cargo.toml", "tests/fixtures/delivery-probe/Cargo.lock", "tests/fixtures/delivery-probe/src/main.rs", "crates/node/src/net/pkarr.rs", "crates/node/src/delivery/wallet.rs", "crates/node/src/client.rs", "scripts/check-delivery.py", "scripts/install-delivery-toolchain.py"):
        selected = make_plan([path])
        assert selected["scopes"] == ["delivery"] and selected["delivery"]
        assert selected["rust_packages"] == expected["delivery"] and selected["web"]
        assert "delivery" in selected["required_jobs"]
        assert "package" in selected["required_jobs"] and selected["package_matrix"]["include"][0]["target"] == "x86_64-unknown-linux-gnu"
    assert not docs["delivery"] and not web["delivery"]
    for path in ("Cargo.toml", "Cargo.lock", "crates/node/Cargo.toml", ".cargo/config.toml", "rust-toolchain.toml", ".github/workflows/ci.yml", "scripts/ci.py", "new-build-input"):
        assert make_plan([path])["rust_packages"] == list(PACKAGES)
    assert make_plan(["crates/node/web/src/a.tsx", "crates/node/src/api.rs"])["rust_packages"] == ["vibeface"]
    assert make_plan(["crates/node/schema.hcl"])["node"]
    assert make_plan(["install.sh"])["node"]
    for path in ("packages/client/src/avatar/derived-faces.json", "packages/client/src/avatar/face.ts"):
        shared = make_plan([path])
        assert shared["scopes"] == ["node"] and shared["rust_packages"] == ["vibeface"] and shared["web"]
    for event in ("schedule", "workflow_dispatch", "merge_group"):
        assert make_plan(["README.md"], event)["rust_packages"] == list(PACKAGES)
    assert make_plan(["README.md"], "push")["docs_only"]
    assert make_plan(["crates/node/web/src/a.tsx"], "push")["rust_packages"] == []
    assert make_plan(["README.md"], "push", ref_type="tag")["test_tiers"] == ["fast", "slow"]
    assert make_plan(["README.md"], "push", ref_type="tag")["package_matrix"]["include"] == list(PLATFORMS)
    assert not make_plan(["crates/node/src/api.rs"])["npm"]
    assert not make_plan(["crates/node/src/api.rs"], channel="stable")["npm"]
    assert not make_plan(["crates/node/src/api.rs"], channel="rc")["npm"]
    for event in ("pull_request", "push", "merge_group"):
        core = make_plan(["Cargo.lock"], event)
        assert core["test_tiers"] == ["fast"] and not core["check_msrv"] and not core["npm"]
        assert core["package_matrix"]["include"] == list(PLATFORMS[:1])
        assert core["package_mode"] == "web" and "rust-check" in core["required_jobs"]
    for event, ref_type in (("schedule", "branch"), ("workflow_dispatch", "branch"), ("push", "tag")):
        extended = make_plan([], event, ref_type=ref_type)
        assert extended["test_tiers"] == ["fast", "slow"] and extended["check_msrv"] and extended["npm"]
        assert extended["package_matrix"]["include"] == list(PLATFORMS)
        assert extended["package_mode"] == "node"
    needs = {job: {"result": "success" if job in web["required_jobs"] else "skipped"} for job in JOBS}
    needs["plan"] = {"result": "success", "outputs": {"plan": json.dumps(web)}}
    gate(needs)
    for result in ("failure", "cancelled", "skipped", "missing"):
        broken = json.loads(json.dumps(needs))
        broken["web"] = {"result": result}
        expect_failure(lambda: gate(broken))
    broken = json.loads(json.dumps(needs))
    broken["plan"]["result"] = "skipped"
    expect_failure(lambda: gate(broken))
    www_needs = {job: {"result": "success" if job in www["required_jobs"] else "skipped"} for job in JOBS}
    www_needs["plan"] = {"result": "success", "outputs": {"plan": json.dumps(www)}}
    gate(www_needs)
    broken = json.loads(json.dumps(www_needs))
    del broken["www"]
    expect_failure(lambda: gate(broken))
    broken = json.loads(json.dumps(needs))
    broken["rust-slow"]["result"] = "failure"
    expect_failure(lambda: gate(broken))
    weakened = dict(web, required_jobs=["quick-check"])
    expect_failure(lambda: gate(needs, weakened))
    ready = make_plan([], "workflow_dispatch")
    expect_failure(lambda: gate(needs, dict(ready, test_tiers=["fast"])))
    expect_failure(lambda: gate(needs, dict(ready, package_matrix={"include": list(PLATFORMS[:1])})))
    expect_failure(lambda: gate(needs, dict(ready, npm=False)))
    expect_failure(lambda: gate(needs, dict(ready, check_msrv=False)))
    expect_failure(lambda: gate(needs, dict(ready, delivery=False)))
    complete = {job: {"result": "success"} for job in JOBS}
    complete["plan"] = {"result": "success", "outputs": {"plan": json.dumps(ready)}}
    gate(complete)
    for job in ready["required_jobs"]:
        incomplete = json.loads(json.dumps(complete))
        incomplete[job]["result"] = "skipped"
        expect_failure(lambda: gate(incomplete))
    expect_failure(lambda: gate(complete, delivery_live=True))
    complete["delivery-live"] = {"result": "success"}
    gate(complete, delivery_live=True)
    for result in ("failure", "cancelled", "skipped", "missing"):
        broken = json.loads(json.dumps(complete))
        broken["delivery-live"] = {"result": result}
        expect_failure(lambda: gate(broken, delivery_live=True))
    expect_failure(lambda: gate(needs, delivery_live=True))
    delivery_plan = make_plan(["proofs/delivery/fixtures/manifest.json"])
    delivery_needs = {job: {"result": "success" if job in delivery_plan["required_jobs"] else "skipped"} for job in JOBS}
    delivery_needs["plan"] = {"result": "success", "outputs": {"plan": json.dumps(delivery_plan)}}
    gate(delivery_needs)
    for result in ("failure", "cancelled", "skipped", "missing"):
        broken = json.loads(json.dumps(delivery_needs))
        broken["delivery"] = {"result": result}
        expect_failure(lambda: gate(broken))
    broken = json.loads(json.dumps(delivery_needs))
    del broken["delivery"]
    expect_failure(lambda: gate(broken))
    assert validate_slow_tests(["other::test", *SLOW_TESTS]) == {"other::test"}
    expect_failure(lambda: validate_slow_tests(["other::test", *SLOW_TESTS[:-1]]))
    expect_failure(lambda: validate_slow_tests(["other::test", *SLOW_TESTS, SLOW_TESTS[0] + "_extra"]))
    expect_failure(lambda: package_args(["vibeface", "vibeface"]))
    scratch = Path(__file__).resolve().parent.parent / ".artifacts/ci-self-test"
    scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="git-", dir=scratch) as directory:
        git("init", "--quiet", cwd=directory)
        source = Path(directory) / "crates/node/src/old.rs"
        source.parent.mkdir(parents=True)
        source.write_text("test fixture\n")
        (Path(directory) / "README.md").write_text("docs\n")
        git("add", ".", cwd=directory)
        git("-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false", "-c", "user.name=CI self-test", "-c", "user.email=ci@example.invalid", "commit", "--quiet", "-m", "base", cwd=directory)
        base = git("rev-parse", "HEAD", cwd=directory).decode().strip()
        target = Path(directory) / "crates/node/web/old.rs"
        target.parent.mkdir(parents=True)
        source.rename(target)
        (Path(directory) / "README.md").unlink()
        git("add", "-A", cwd=directory)
        git("-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false", "-c", "user.name=CI self-test", "-c", "user.email=ci@example.invalid", "commit", "--quiet", "-m", "rename and delete", cwd=directory)
        paths = changed_paths(base, "HEAD", "pull_request", cwd=directory)
        assert set(paths) == {"README.md", "crates/node/src/old.rs", "crates/node/web/old.rs"}
        assert make_plan(paths)["rust_packages"] == ["vibeface"]
        pr_head = git("rev-parse", "HEAD", cwd=directory).decode().strip()
        git("checkout", "--quiet", "--detach", base, cwd=directory)
        upstream = Path(directory) / "docs/upstream.md"
        upstream.parent.mkdir()
        upstream.write_text("base moved\n")
        git("add", ".", cwd=directory)
        git("-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false", "-c", "user.name=CI self-test", "-c", "user.email=ci@example.invalid", "commit", "--quiet", "-m", "advance base", cwd=directory)
        new_base = git("rev-parse", "HEAD", cwd=directory).decode().strip()
        assert set(changed_paths(new_base, pr_head, "pull_request", cwd=directory)) == set(paths)
        event_file = Path(directory) / "event.json"
        arguments = argparse.Namespace(event=None, base=new_base, head=pr_head)
        environment = {"GITHUB_EVENT_NAME": "pull_request", "GITHUB_REF_TYPE": "branch", "GITHUB_EVENT_PATH": str(event_file)}
        with patch.dict(os.environ, environment), patch(f"{__name__}.changed_paths", return_value=paths) as diff, patch(f"{__name__}.release_metadata", return_value={"channel": "nightly"}):
            for draft in (True, False):
                event_file.write_text(json.dumps({"pull_request": {"base": {"sha": new_base}, "head": {"sha": pr_head}, "draft": draft}}))
                arguments.base = None
                selected = event_plan(arguments)
                diff.assert_called_with(new_base, pr_head, "pull_request")
                assert selected == make_plan(paths)
                assert selected["test_tiers"] == ["fast"]
            event_file.write_text(json.dumps({"pull_request": {"base": {"sha": new_base}, "head": {"sha": pr_head}, "labels": [{"name": NIGHTLY_LABEL}]}}))
            selected = event_plan(arguments)
            assert selected["nightly"] and selected["required_jobs"] == []
        nightly_needs = {job: {"result": "skipped"} for job in JOBS}
        nightly_needs["plan"] = {"result": "success", "outputs": {"plan": json.dumps(nightly)}}
        gate(nightly_needs)
        for version, channel in (("1.2.3", "stable"), ("1.2.3-rc.2", "rc"), ("1.2.3-nightly.20261019.1", "nightly")):
            cargo_metadata = {"packages": [{"name": "vibeface", "version": version, "rust_version": "1.89"}]}
            with patch("subprocess.check_output", return_value=json.dumps(cargo_metadata)), patch(f"{__name__}.git") as ancestor:
                assert release_metadata("branch", "")["channel"] == channel
                assert release_metadata("tag", f"v{version}")["prerelease"] == (channel != "stable")
                ancestor.assert_called_once_with("merge-base", "--is-ancestor", "HEAD", "origin/main")
                expect_failure(lambda: release_metadata("tag", "v0.0.0"))
        for version in ("01.2.3", "1.2.3-nightly.20260229.1", "1.2.3-nightly.20261019.0", "1.2.3+build"):
            cargo_metadata = {"packages": [{"name": "vibeface", "version": version, "rust_version": "1.89"}]}
            with patch("subprocess.check_output", return_value=json.dumps(cargo_metadata)):
                expect_failure(lambda: release_metadata("branch", ""))
    for check_msrv in (False, True):
        with patch("subprocess.run") as run:
            run_check(["vibeface"], "1.89" if check_msrv else None)
            commands = [call.args[0] for call in run.call_args_list]
            assert commands[0] == ["cargo", "clippy", "--all-targets", "--locked", "-p", "vibeface", "--", "-D", "warnings"]
            assert commands[1:] == ([
                ["rustup", "toolchain", "install", "1.89", "--profile", "minimal"],
                ["cargo", "+1.89", "check", "--locked", "-p", "vibeface"],
            ] if check_msrv else [])
    root = Path(__file__).resolve().parent.parent
    subprocess.run(["python3", str(root / "scripts/build-inputs.py"), "self-test"], check=True)
    config = (root / "crates/node/src/config.rs").read_text()
    protocol, epoch = (re.search(rf"^pub const {name}: u32 = (\d+);$", config, re.M)[1] for name in ("UPDATE_PROTOCOL", "DATA_EPOCH"))
    assert f'info["update_protocol"]=="{protocol}" && info["data_epoch"]=="{epoch}"' in (root / "install.sh").read_text(), "install.sh accepts the build's update protocol and data epoch"
    release_privacy_test(root, scratch)
    gatekeeper_test(root, scratch)
    print("CI selector, event, test partition, check commands, rename/delete, gate and installer epoch self-tests passed")


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
            binary = Path(os.environ["PACKAGE"]) / "vibeface"
            if tool == "codesign":
                if sys.argv[1:] == ["--verify", "--strict", str(binary)]:
                    sys.exit(0)
                assert sys.argv[1:] == ["-dvvv", str(binary)]
                print("Identifier=cn.leaperone.vibeface\\nTeamIdentifier=FIXTURETEAM\\nCodeDirectory flags=0x10000(runtime)")
            elif tool == "spctl":
                assert sys.argv[1:] == ["-a", "-vvv", "--ignore-cache", "--no-cache", "-t", "open", "--context", "context:primary-signature", str(binary)]
                state = Path(os.environ["RUNNER_TEMP"]) / "calls"
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
            environment = dict(os.environ, PATH=f"{tools}{os.pathsep}{os.environ['PATH']}", PACKAGE=str(case), RUNNER_TEMP=str(case), VIBEFACE_CODESIGN_TEAM="FIXTURETEAM", FIXTURE_ASSESSMENTS=json.dumps(assessments), FIXTURE_MUTATE=json.dumps(mutate))
            result = subprocess.run(["bash", "--noprofile", "--norc", "-e", "-o", "pipefail"], input=script, env=environment, text=True, capture_output=True)
            assert result.returncode == expected, (name, result.returncode, result.stdout, result.stderr)
            assert int((case / "calls").read_text()) == len(assessments), name
            sleeps = [int(line) for line in (case / "sleeps").read_text().split()] if (case / "sleeps").exists() else []
            assert len(sleeps) == len(assessments) - 1 and sleeps == sorted(sleeps), (name, sleeps)
            if name == "pending-exhausted":
                assert 600 <= sum(sleeps) <= 900, ("waits long enough for Apple's ticket propagation", sleeps)
    print("Gatekeeper workflow control-flow self-tests passed; native assessment remains required")


def release_privacy_test(root, scratch):
    import shutil
    import tarfile

    with tempfile.TemporaryDirectory(prefix="release-", dir=scratch) as directory:
        repo = Path(directory)
        for name in ("scripts/package-node.sh", "scripts/release-rustflags.py", "scripts/build-inputs.py", "scripts/install-node-service.sh", "Cargo.toml", "crates/node/src/config.rs", "README.md"):
            target = repo / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(root / name, target)
        source = repo / "private build.rs"
        source.write_text('#[cfg(not(privacy_flag))] compile_error!("caller flag lost"); fn main() { println!("{}", file!()); }')
        binary = repo / "target/privacy-fixture/release/vibeface"
        binary.parent.mkdir(parents=True)
        environment = {**os.environ, "CARGO_TARGET_DIR": str(repo / "target"), "RUSTFLAGS": "--cfg privacy_flag"}
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
        (repo / "docs").mkdir()
        (repo / "docs/guide.md").write_text("public guide")
        (repo / ".gitignore").write_text(".artifacts/\ntarget/\nbin/\nprivate build.rs\ncrates/node/web/generated/\ncrates/node/web/dist/\n.env\ncargo-called\n")
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        subprocess.run(["git", "add", "."], cwd=repo, check=True)

        def generated():
            tool = ["python3", str(repo / "scripts/build-inputs.py")]
            for component, files in (
                ("crypto", {"vibeface_secure.js": "fixture", "vibeface_secure_bg.wasm": "fixture"}),
                ("web", {"_shell.html": "fixture"}),
            ):
                subprocess.run([*tool, "prepare", component], check=True)
                output = repo / ("crates/node/web/generated" if component == "crypto" else "crates/node/web/dist/client")
                for name, content in files.items():
                    (output / name).write_text(content)
                subprocess.run([*tool, "finish", component], check=True)

        generated()
        (repo / "docs/.env").write_text("PRIVATE_FIXTURE=must-not-be-packaged")
        (repo / "docs/untracked-note.md").write_text("must-not-be-packaged")
        if hasattr(os, "setxattr"):
            os.setxattr(repo / "docs/guide.md", "user.build-host", b"private builder")
        environment.update({name: "" for name in ("VIBEFACE_CODESIGN_IDENTITY", "VIBEFACE_NOTARY_KEY", "VIBEFACE_NOTARY_KEY_ID", "VIBEFACE_NOTARY_ISSUER")})
        fake_cargo = repo / "bin/cargo"
        fake_cargo.parent.mkdir()
        fake_cargo.write_text('#!/bin/sh\ntouch "$CARGO_MARKER"\n')
        fake_cargo.chmod(0o755)
        marker = repo / "cargo-called"
        environment.update(PATH=f"{fake_cargo.parent}{os.pathsep}{environment['PATH']}", CARGO_MARKER=str(marker))
        subprocess.run(["bash", str(repo / "scripts/package-node.sh"), "--target", "privacy-fixture"], env=environment, check=True, stdout=subprocess.PIPE)
        assert marker.exists(), "packaging must always rebuild the native binary"
        archive_path, = (repo / ".artifacts/packages").glob("*.tar.gz")
        with tarfile.open(archive_path) as archive:
            for entry in archive.getmembers():
                assert entry.uid == entry.gid == 0 and entry.uname == entry.gname == ""
                assert not set(entry.pax_headers) - {"mtime"}
                assert not any(part.startswith("._") for part in Path(entry.name).parts)
            package = archive.getmembers()[0].name
            assert archive.extractfile(f"{package}/docs/guide.md").read() == b"public guide"
            assert not any(entry.name.endswith(("/.env", "/untracked-note.md")) for entry in archive.getmembers())
            assert archive.getmember(f"{package}/vibeface").mode & 0o111
        (repo / "scripts/release-rustflags.py").write_text('raise RuntimeError("fixture failure")')
        generated()
        marker.unlink()
        assert subprocess.run(["bash", str(repo / "scripts/package-node.sh"), "--target", "privacy-fixture"], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE).returncode != 0
        assert not marker.exists(), "a failed privacy setup must stop the build"
    print("Release privacy self-test passed: caller flags, source paths, archive owners and extended attributes")


def expect_failure(check):
    try:
        check()
    except ValueError:
        return
    raise AssertionError("Expected rejection")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    commands = parser.add_subparsers(dest="command")
    plan = commands.add_parser("plan")
    plan.add_argument("--base")
    plan.add_argument("--head")
    plan.add_argument("--event", choices=("pull_request", "push", "schedule", "workflow_dispatch", "merge_group"))
    plan.add_argument("--github-output")
    final_gate = commands.add_parser("gate")
    final_gate.add_argument("--plan", type=Path)
    final_gate.add_argument("--delivery-live", action="store_true", help="also require the explicitly dispatched live proof job")
    needs = final_gate.add_mutually_exclusive_group(required=True)
    needs.add_argument("--needs", type=Path)
    needs.add_argument("--needs-env")
    for name in ("test", "check"):
        command = commands.add_parser(name)
        selection = command.add_mutually_exclusive_group(required=True)
        selection.add_argument("--packages", nargs="+")
        selection.add_argument("--plan-env")
        if name == "test":
            command.add_argument("--tier", choices=("fast", "slow"), required=True)
        else:
            command.add_argument("--msrv", help="also check the declared minimum Rust version")
    args = parser.parse_args()
    if args.self_test:
        self_test()
    elif args.command == "plan":
        write_plan(event_plan(args), args.github_output)
    elif args.command == "gate":
        needs = json.loads(args.needs.read_text() if args.needs else os.environ[args.needs_env])
        gate(needs, json.loads(args.plan.read_text()) if args.plan else None, delivery_live=args.delivery_live)
    elif args.command in {"test", "check"}:
        packages = args.packages if args.packages else json.loads(os.environ[args.plan_env])["rust_packages"]
        if args.command == "test":
            run_test(packages, args.tier)
        else:
            plan = json.loads(os.environ[args.plan_env]) if args.plan_env else None
            if plan and plan["check_msrv"] and not args.msrv:
                parser.error("The selected CI plan requires --msrv")
            run_check(packages, args.msrv if plan is None or plan["check_msrv"] else None)
    else:
        parser.error("Select a command or --self-test")


if __name__ == "__main__":
    main()
