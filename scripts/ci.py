#!/usr/bin/env python3
"""Select CI coverage once, run its Rust test tiers, and check the final gate."""

import argparse
import datetime
import fnmatch
import http.client
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import urllib.request


PACKAGES = ("vibeface-protocol", "vibeface-bridge", "vibeface-secure", "vibeface-core", "vibeface-control", "vibeface", "vibeface-secure-ffi", "vibeface-delivery")
DEPENDENTS = {
    "protocol": ("bridge", "control"),
    "bridge": ("control",),
    "secure": ("node", "secure-ffi"),
    "node": (),
    "control": ("node", "cli"),
    "cli": (),
    "secure-ffi": (),
    "delivery": ("node",),
}
CRATES = {
    "protocol": "vibeface-protocol",
    "bridge": "vibeface-bridge",
    "secure": "vibeface-secure",
    "node": "vibeface-core",
    "control": "vibeface-control",
    "cli": "vibeface",
    "secure-ffi": "vibeface-secure-ffi",
    "delivery": "vibeface-delivery",
}
PATH_RULES = (
    ("delivery", ("crates/delivery/*", "crates/delivery/proof/*", "contracts/delivery/*", "tests/fixtures/delivery-probe/*", "apps/core/src/delivery/*", "apps/core/src/client.rs", "apps/core/src/config.rs", "apps/core/src/main.rs", "apps/core/src/maintenance.rs", "apps/core/src/net.rs", "apps/core/src/net/mesh/record.rs", "apps/core/src/runtime.rs", "apps/core/src/runtime/peers.rs", "scripts/check-delivery.py", "scripts/install-delivery-toolchain.py", "scripts/run-delivery-live.py")),
    ("full", ("Cargo.toml", "Cargo.lock", "crates/*/Cargo.toml", "apps/core/Cargo.toml", "apps/cli/Cargo.toml", "crates/control/Cargo.toml", "rust-toolchain*", ".cargo/*", ".github/*", ".preflight.toml", "scripts/ci.py")),
    ("www", ("apps/www/*",)),
    ("docs", ("README.md", "README.zh-CN.md", "AGENTS.md", "CLAUDE.md", "packages/docs/*")),
    ("node", ("config/public-bootstrap.json", "packages/client/src/avatar/derived-faces.json", "packages/client/src/avatar/face.ts")),
    ("desktop", ("apps/desktop/src/*", "apps/desktop/tests/*")),
    ("mobile", ("apps/mobile/*", "packages/secure-native/*")),
    ("web", ("apps/app/*", "packages/client/*", "packages/theme/*", "package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml", "scripts/build-web.sh", "scripts/build-web-crypto.sh")),
    ("protocol", ("crates/protocol/*",)),
    ("bridge", ("crates/bridge/*",)),
    ("secure", ("crates/secure/*", "scripts/crypto-client.mjs")),
    ("secure-ffi", ("crates/secure-ffi/*", "scripts/build-mobile-secure.sh")),
    ("node", ("apps/core/*", "install.sh", "scripts/package-node.sh", "scripts/package-node-windows.py",
              "scripts/install-node-service.sh", "scripts/install-node-service.ps1", "scripts/verify-node.py",
              "scripts/verify-windows-node.py")),
    ("control", ("crates/control/*",)),
    ("cli", ("apps/cli/*",)),
)
WWW_PATHS = ("apps/www/*", "packages/docs/*", "packages/theme/*", "package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml")
MOBILE_PATHS = ("packages/client/*", "packages/theme/*", "package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml")
DESKTOP_PATHS = ("package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml")
SLOW_TESTS = (
    "store::tests::append_100k_stays_flat",
    "runtime::peers::tests::a_round_left_behind_pulls_again_without_waiting_for_hello",
)
PLATFORMS = (
    {"runner": "ubuntu-24.04", "target": "x86_64-unknown-linux-gnu"},
    {"runner": "ubuntu-24.04-arm", "target": "aarch64-unknown-linux-gnu"},
    {"runner": "macos-15", "target": "aarch64-apple-darwin"},
    {"runner": "windows-2025", "target": "x86_64-pc-windows-msvc"},
)
JOBS = ("quick-check", "crypto-web", "web", "mobile-check", "desktop-check", "rust-check", "rust-fast", "rust-slow", "delivery", "package", "npm-package", "npm-smoke", "www")
# Only an explicit dispatch runs the hosted Windows and desktop application checks.
DESKTOP_JOBS = ["windows-native", "desktop-macos", "desktop-windows"]
LOCAL_PROCESS_RULES = (
    ("memory-sync", ("apps/core/src/store/bot_settings.rs", "apps/core/src/store/reduce.rs", "apps/core/src/store/log.rs", "apps/core/src/store/sync.rs", "apps/core/src/agent_context.rs", "apps/core/src/identity*", "crates/secure/*")),
    ("auto-route", ("config/public-bootstrap.json", "apps/core/src/identity*", "apps/core/src/crypto.rs", "apps/core/src/local_access.rs", "apps/core/src/net*", "crates/secure/*", "crates/control/*")),
    ("default", ("config/public-bootstrap.json", "apps/core/src/net*", "apps/core/src/runtime/peers.rs", "apps/core/src/store/sync.rs", "apps/core/src/store/log.rs", "apps/core/src/store/mailbox.rs", "apps/core/src/fetch.rs", "apps/core/src/identity*", "crates/secure/*")),
    ("phase1", ("apps/core/src/manager.rs", "apps/core/src/schedul*", "apps/core/src/work.rs", "apps/core/src/runtime/work.rs", "apps/core/src/runtime/followup.rs", "apps/core/src/runtime/schedule_delivery.rs", "apps/core/src/runtime/manager_control.rs", "apps/core/src/store/work.rs", "apps/core/src/store/scheduler.rs", "apps/core/src/store/mailbox.rs", "apps/core/src/store/reduce.rs", "apps/core/src/store/log.rs")),
    ("chat-agent", ("crates/bridge/*", "crates/control/*", "apps/cli/*", "apps/core/src/runtime/codex_server.rs", "apps/core/src/runtime/claude.rs", "apps/core/src/runtime/pi.rs", "apps/core/src/runtime/opencode.rs", "apps/core/src/runtime/provider_adapter.rs", "apps/core/src/runtime/provider_probe.rs", "apps/core/src/runtime/agent_tools.rs", "apps/core/src/runtime/chat_controls.rs", "apps/core/src/runtime/executor.rs", "apps/core/src/runtime/observation.rs", "apps/core/src/runtime/triage.rs", "apps/core/src/runtime/work.rs", "apps/core/src/runtime/followup.rs", "apps/core/src/runtime/bots.rs", "apps/core/src/runtime/manager_control.rs", "apps/core/src/agent_context.rs", "apps/core/src/providers.rs", "apps/core/src/triage.rs", "apps/core/src/tools.rs", "apps/core/src/work.rs", "apps/core/src/manager.rs", "apps/core/src/control.rs", "apps/core/src/store/triage.rs", "apps/core/src/store/questions.rs", "apps/core/src/store/work.rs", "apps/core/src/store/mailbox.rs", "apps/core/src/store/bots.rs")),
    ("phase4", ("crates/bridge/*", "crates/control/*", "apps/core/src/net*", "apps/core/src/runtime/peers.rs", "apps/core/src/runtime/work.rs", "apps/core/src/runtime/agent_tools.rs", "apps/core/src/runtime/manager_control.rs", "apps/core/src/work.rs", "apps/core/src/tools.rs", "apps/core/src/store/work.rs", "apps/core/src/store/mailbox.rs", "apps/core/src/store/sync.rs", "apps/core/src/store/bots.rs", "apps/core/src/identity*", "apps/core/src/fetch.rs")),
    ("public-node", ("config/public-bootstrap.json", "apps/core/src/net*", "apps/core/src/runtime/peers.rs", "apps/core/src/identity*", "apps/core/src/local_access.rs", "apps/core/src/crypto.rs", "crates/control/*")),
    ("mesh", ("config/public-bootstrap.json", "apps/core/src/net*", "apps/core/src/runtime/peers.rs")),
    ("phase5", ("apps/core/src/store/*", "apps/core/src/identity*", "apps/core/src/maintenance.rs", "apps/core/src/logging.rs", "crates/control/*", "crates/secure/src/recovery.rs")),
    ("delivery-charity", PATH_RULES[0][1]),
)


def local_process_modes(paths, full):
    ordinary = {mode for mode, _ in LOCAL_PROCESS_RULES if mode != "delivery-charity"}
    selected = {mode for mode, _ in LOCAL_PROCESS_RULES} if full else set()
    for path in paths:
        matched = {mode for mode, patterns in LOCAL_PROCESS_RULES if any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns)}
        selected.update(matched)
        if not matched & ordinary and (path.startswith(("apps/core/", "apps/cli/", "crates/")) or any(fnmatch.fnmatchcase(path, pattern) for scope, patterns in PATH_RULES if scope == "node" for pattern in patterns)):
            selected.update(ordinary)
    return [mode for mode, _ in LOCAL_PROCESS_RULES if mode in selected]


def make_plan(paths, event="pull_request", ref_type="branch", channel="nightly"):
    scopes = set()
    for path in paths:
        scopes.add(next((scope for scope, patterns in PATH_RULES if any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns)), "full"))
        if any(fnmatch.fnmatchcase(path, pattern) for pattern in WWW_PATHS):
            scopes.add("www")
        if any(fnmatch.fnmatchcase(path, pattern) for pattern in MOBILE_PATHS):
            scopes.add("mobile")
        if any(fnmatch.fnmatchcase(path, pattern) for pattern in DESKTOP_PATHS):
            scopes.add("desktop")
        if path.startswith("apps/desktop/src/shared/"):
            scopes.add("web")
    plan = scope_plan(scopes, event, ref_type, channel)
    return dict(plan, changed_files=len(paths), local_process_modes=local_process_modes(paths, plan["full"]))


def scope_plan(scopes, event, ref_type, channel):
    if (
        scopes - {"full", "docs", "web", "www", "mobile", "desktop", *DEPENDENTS}
        or event not in {"pull_request", "push", "schedule", "workflow_dispatch", "merge_group"}
        or ref_type not in {"branch", "tag"}
        or channel not in {"stable", "rc", "nightly"}
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
    # Mobile behavior tests still live in the Web suite.
    web = full or bool(scopes & {"web", "mobile"}) or bool(affected & {"node", "cli", "control"})
    www = full or "www" in scopes
    mobile = full or "mobile" in scopes
    desktop = full or "desktop" in scopes
    slow = "node" in affected and extended
    delivery = full or "delivery" in affected
    platforms = PLATFORMS if "node" in affected and extended else PLATFORMS[:1]
    plan = {
        "full": full,
        "extended": extended,
        "check_msrv": bool(packages) and extended,
        "docs_only": not web and not packages and not mobile and not desktop,
        "event": event,
        "ref_type": ref_type,
        "channel": channel,
        "scopes": sorted(scopes),
        "web": web,
        "www": www,
        "mobile": mobile,
        "desktop": desktop,
        "node": "node" in affected,
        "delivery": delivery,
        "rust_packages": packages,
        "test_tiers": (["fast"] if packages else []) + (["slow"] if slow else []),
        "package_mode": "node" if "node" in affected and extended else "web",
        "package_matrix": {"include": list(platforms) if web else []},
        "npm": channel == "nightly" and "node" in affected and extended,
    }
    required = {"quick-check"}
    if web:
        required.update(("crypto-web", "web", "package"))
    if mobile:
        required.add("mobile-check")
    if desktop:
        required.add("desktop-check")
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


def automatic_main_push(event):
    return os.environ.get("GITHUB_ACTIONS") == "true" and event == "push" and os.environ.get("GITHUB_EVENT_NAME") == "push" and os.environ.get("GITHUB_REF") == "refs/heads/main"


def successful_push_base(head):
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    token = os.environ.get("GH_TOKEN")
    current_run = os.environ.get("GITHUB_RUN_ID", "")
    if not token or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) or not current_run.isdigit():
        return None
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repository}/actions/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=100",
        headers={"Accept": "application/vnd.github+json", "Authorization": f"Bearer {token}", "X-GitHub-Api-Version": "2022-11-28"},
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            runs = json.load(response)["workflow_runs"]
        if not isinstance(runs, list):
            raise ValueError("Invalid workflow runs")
        for run in runs:
            if not isinstance(run, dict):
                raise ValueError("Invalid workflow run")
            if (run["repository"]["full_name"] != repository or run["path"] != ".github/workflows/ci.yml"
                    or run["head_branch"] != "main" or run["event"] != "push"
                    or run["status"] != "completed" or run["conclusion"] != "success"):
                continue
            if type(run["id"]) is not int or run["id"] <= 0 or not isinstance(run["head_sha"], str) or not re.fullmatch(r"[0-9a-f]{40}", run["head_sha"]):
                raise ValueError("Invalid successful workflow run identity")
            if run["id"] == int(current_run) or run["head_sha"] == head:
                continue
            git("merge-base", "--is-ancestor", run["head_sha"], head)
            return run["head_sha"], run["id"]
    except (OSError, http.client.HTTPException, ValueError, KeyError, TypeError, subprocess.CalledProcessError):
        return None
    return None


def event_plan(args):
    event = args.event or os.environ.get("GITHUB_EVENT_NAME", "pull_request")
    ref_type = os.environ.get("GITHUB_REF_TYPE", "branch")
    payload_path = os.environ.get("GITHUB_EVENT_PATH")
    payload = json.loads(Path(payload_path).read_text()) if payload_path else {}
    base, head = args.base, args.head
    reason, baseline_run = "event-range", None
    forced = event in {"schedule", "workflow_dispatch", "merge_group"} or ref_type == "tag"
    if not base and event == "pull_request":
        pr = payload["pull_request"]
        base, head = pr["base"]["sha"], pr["head"]["sha"]
    elif not base and automatic_main_push(event):
        head = os.environ["GITHUB_SHA"]
        if payload.get("after") != head:
            raise ValueError("Push payload differs from the current commit")
        baseline = successful_push_base(head)
        base, baseline_run = baseline if baseline else (None, None)
        reason = "successful-main-push" if baseline else "no-trusted-main-push"
    elif not base and event == "push":
        base, head = payload.get("before"), payload.get("after")
    known_range = bool(base and head and set(base) != {"0"})
    head = git("rev-parse", "--verify", "--end-of-options", f"{head or 'HEAD'}^{{commit}}").decode().strip()
    if forced:
        paths, base, reason = [], None, "full-event"
    elif not known_range:
        paths, base = ["__unknown_build_inputs__"], None
        if reason != "no-trusted-main-push":
            reason = "unknown-range"
    else:
        base = git("rev-parse", "--verify", "--end-of-options", f"{base}^{{commit}}").decode().strip()
        paths = changed_paths(base, head, event)
    metadata = release_metadata(ref_type, os.environ.get("GITHUB_REF_NAME", ""))
    plan = make_plan(paths, event, ref_type, metadata["channel"])
    plan.update(metadata)
    plan.update(coverage_base=base, coverage_head=head, coverage_reason=reason, baseline_run_id=baseline_run)
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
            "mobile": str(plan["mobile"]).lower(),
            "desktop": str(plan["desktop"]).lower(),
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
        }
        with open(output, "a") as destination:
            destination.writelines(f"{key}={value}\n" for key, value in outputs.items())
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as destination:
            destination.write(
                "### Selected CI coverage\n\n"
                f"- Frozen range: {plan['coverage_base'] or 'full coverage'} .. {plan['coverage_head']} ({plan['coverage_reason']})\n"
                f"- Changed paths: {plan['changed_files']}\n"
                f"- Full workspace scope: {plan['full']}\n"
                f"- Coverage tier: {'extended' if plan['extended'] else 'core'}\n"
                f"- Minimum Rust version check: {plan['check_msrv']}\n"
                f"- Rust packages: {', '.join(plan['rust_packages']) or 'none'}\n"
                f"- Test tiers: {', '.join(plan['test_tiers']) or 'none'}\n"
                f"- Package acceptance: {plan['package_mode'] if plan['web'] else 'none'}\n"
                f"- Required jobs: {', '.join(plan['required_jobs'])}\n"
                "\nLocal process checks to run separately, using current CLI/Core and JS/WASM paths:\n\n"
            )
            for mode in plan["local_process_modes"]:
                flag = "" if mode == "default" else f" --{mode}"
                if mode == "delivery-charity":
                    flag += ' --delivery-probe "$probe"'
                destination.write(f'- `python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-{mode}{flag}`\n')
            if not plan["local_process_modes"]:
                destination.write("- None for these paths.\n")


def frozen_plan(plan):
    if os.environ.get("GITHUB_ACTIONS") == "true":
        if plan["event"] != os.environ.get("GITHUB_EVENT_NAME") or plan["ref_type"] != os.environ.get("GITHUB_REF_TYPE") or plan["coverage_head"] != os.environ.get("GITHUB_SHA"):
            raise ValueError("CI plan differs from the current event or commit")
    head, base, reason = plan["coverage_head"], plan["coverage_base"], plan["coverage_reason"]
    if not isinstance(head, str) or not re.fullmatch(r"[0-9a-f]{40}", head) or git("rev-parse", "HEAD").decode().strip() != head:
        raise ValueError("CI coverage must use the checked-out commit")
    forced = plan["event"] in {"schedule", "workflow_dispatch", "merge_group"} or plan["ref_type"] == "tag"
    if forced:
        if base is not None or reason != "full-event" or plan["baseline_run_id"] is not None:
            raise ValueError("Invalid full-event coverage")
        paths = []
    elif base is None:
        if reason not in {"no-trusted-main-push", "unknown-range"} or plan["baseline_run_id"] is not None:
            raise ValueError("Invalid full coverage fallback")
        paths = ["__unknown_build_inputs__"]
    else:
        if not isinstance(base, str) or not re.fullmatch(r"[0-9a-f]{40}", base):
            raise ValueError("CI coverage requires a fixed base commit")
        if automatic_main_push(plan["event"]):
            if reason != "successful-main-push" or type(plan["baseline_run_id"]) is not int or plan["baseline_run_id"] <= 0 or base == head:
                raise ValueError("Invalid successful main push baseline")
            git("merge-base", "--is-ancestor", base, head)
        elif reason != "event-range" or plan["baseline_run_id"] is not None:
            raise ValueError("Invalid event coverage range")
        paths = changed_paths(base, head, plan["event"])
    if automatic_main_push(plan["event"]) and reason not in {"successful-main-push", "no-trusted-main-push"}:
        raise ValueError("Main push coverage requires a successful ancestor or full fallback")
    return make_plan(paths, plan["event"], plan["ref_type"], plan["channel"])


def gate(needs, plan=None, delivery_live=False, desktop=False):
    if needs.get("plan", {}).get("result") != "success":
        raise ValueError("CI plan did not succeed")
    if plan is None:
        plan = json.loads(needs["plan"]["outputs"]["plan"])
    if "coverage_head" in plan or os.environ.get("GITHUB_ACTIONS") == "true":
        try:
            expected = frozen_plan(plan)
        except (KeyError, TypeError, subprocess.CalledProcessError) as error:
            raise ValueError("Invalid frozen CI coverage") from error
    else:
        expected = scope_plan(set(plan["scopes"]), plan["event"], plan["ref_type"], plan["channel"])
    if any(plan.get(key) != value for key, value in expected.items()):
        raise ValueError("Required coverage differs from the selected CI scope and scenario")
    if delivery_live and plan["event"] != "workflow_dispatch":
        raise ValueError("Live delivery proofs require explicit workflow_dispatch")
    if desktop and plan["event"] != "workflow_dispatch":
        raise ValueError("Windows native and desktop application checks require explicit workflow_dispatch")
    required = expected["required_jobs"] + (["delivery-live"] if delivery_live else []) + (DESKTOP_JOBS if desktop else [])
    failed = [job for job in required if needs.get(job, {}).get("result") != "success"]
    failed.extend(job for job in (*JOBS, "delivery-live", *DESKTOP_JOBS)
                  if job not in required and needs.get(job, {}).get("result") in {"failure", "cancelled"})
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
    test_package = "vibeface-core" if "vibeface-core" in packages else None
    if test_package:
        listing_args = selected if tier == "fast" else ["-p", test_package, "--lib"]
        listing = subprocess.check_output(["cargo", "test", "--locked", *listing_args, "--", "--list"], text=True)
        names = [line[:-6] for line in listing.splitlines() if line.endswith(": test")]
        fast = validate_slow_tests(names)
        print(f"Partition: {len(fast)} fast test names, {len(SLOW_TESTS)} exact slow tests", flush=True)
    if tier == "slow":
        if not test_package:
            raise ValueError("Slow tier requires the Core crate")
        command = ["cargo", "test", "--locked", "-p", test_package, "--lib", "--", "--exact", "--include-ignored", *SLOW_TESTS]
    else:
        skips = [argument for name in SLOW_TESTS for argument in ("--skip", name)]
        command = ["cargo", "test", "--locked", "--no-fail-fast", *selected, "--", *skips]
    subprocess.run(command, check=True)


def run_check(packages, msrv=None):
    selected = package_args(packages)
    subprocess.run(["cargo", "clippy", "--all-targets", "--locked", *selected, "--", "-D", "warnings"], check=True)
    if msrv:
        subprocess.run(["rustup", "toolchain", "install", msrv, "--profile", "minimal"], check=True)
        subprocess.run(["cargo", f"+{msrv}", "check", "--locked", *selected], check=True)


def self_test():
    from unittest.mock import patch
    subprocess.run(["python3", str(Path(__file__).resolve().parent / "check-release-workflow.py"), "--self-test"], check=True)
    subprocess.run(["python3", str(Path(__file__).resolve().parent / "deploy.py"), "self-test"], check=True)
    subprocess.run(["python3", str(Path(__file__).resolve().parent / "deploy-run.py"), "self-test"], check=True)
    subprocess.run(["python3", str(Path(__file__).resolve().parent / "deploy-activate.py"), "--self-test"], check=True)
    subprocess.run(["python3", str(Path(__file__).resolve().parent.parent / "deploy/runtime.py"), "self-test"], check=True)
    subprocess.run(["python3", str(Path(__file__).resolve().parent.parent / "deploy/static-config.py"), "--self-test"], check=True)
    web = make_plan(["apps/app/src/routes/index.tsx"])
    assert web["rust_packages"] == [] and web["test_tiers"] == []
    assert web["required_jobs"] == ["quick-check", "crypto-web", "web", "package"]
    assert web["package_mode"] == "web" and web["package_matrix"]["include"] == list(PLATFORMS[:1])
    assert make_plan(["apps/app/src/content.md"])["web"]
    assert web["local_process_modes"] == []
    bootstrap = make_plan(["config/public-bootstrap.json"])
    assert bootstrap["rust_packages"] == ["vibeface-core"] and bootstrap["web"]
    assert bootstrap["local_process_modes"] == ["auto-route", "default", "public-node", "mesh"]
    www = make_plan(["apps/www/src/routes/index.tsx"])
    assert www["www"] and not www["web"] and not www["rust_packages"]
    assert www["required_jobs"] == ["quick-check", "www"]
    assert make_plan(["packages/client/src/lib/transport.ts", "pnpm-lock.yaml"])["scopes"] == ["desktop", "mobile", "web", "www"]
    assert make_plan(["apps/mobile/app/_layout.tsx", "packages/theme/theme.css"])["scopes"] == ["mobile", "web", "www"]
    for path in ("apps/desktop/src/main/bounded-read.ts", "apps/desktop/tests/bounded-read.test.ts"):
        desktop = make_plan([path], "push")
        assert desktop["scopes"] == ["desktop"] and not desktop["full"]
        assert desktop["required_jobs"] == ["quick-check", "desktop-check"]
        assert not desktop["rust_packages"] and not desktop["web"] and not desktop["delivery"]
        assert not desktop["docs_only"] and not desktop["local_process_modes"]
    for name in ("contract.ts", "client-contract.ts", "node-view.ts"):
        shared = make_plan(["apps/desktop/src/shared/" + name])
        assert shared["scopes"] == ["desktop", "web"]
        assert shared["required_jobs"] == ["quick-check", "crypto-web", "web", "desktop-check", "package"]
    for path in ("apps/mobile/app/_layout.tsx", "packages/secure-native/src/index.ts"):
        mobile = make_plan([path])
        assert mobile["scopes"] == ["mobile"] and mobile["web"]
        assert mobile["required_jobs"] == ["quick-check", "crypto-web", "web", "mobile-check", "package"]
        assert not mobile["rust_packages"] and not mobile["docs_only"]
    for path in ("packages/client/src/lib/transport.ts", "packages/client/src/avatar/face.ts", "packages/client/src/avatar/derived-faces.json", "packages/theme/theme.css", "package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml"):
        assert make_plan([path])["mobile"], path
    assert make_plan(["crates/secure-ffi/src/lib.rs"])["rust_packages"] == ["vibeface-secure-ffi"]
    docs = make_plan(["README.md", "packages/docs/node.md"])
    assert docs["docs_only"] and docs["www"] and docs["required_jobs"] == ["quick-check", "www"]
    for path in ("README.md", "README.zh-CN.md"):
        assert make_plan([path])["required_jobs"] == ["quick-check"]
    assert make_plan(["README.zh-CN.md", "apps/core/src/api.rs"])["rust_packages"] == ["vibeface-core"]
    for path in ("README.zh-CN.md.rs", ".agents/skills/release/SKILL.md", "scripts/release.py", "scripts/daily-nightly.py",
                 "apps/desktop/electron-builder.cjs", "scripts/build-desktop.mjs"):
        assert make_plan([path])["full"]
    assert docs["local_process_modes"] == [] and www["local_process_modes"] == []
    assert make_plan(["apps/core/src/store/bot_settings.rs"])["local_process_modes"] == ["memory-sync", "phase5"]
    assert make_plan(["apps/core/src/agent_context.rs"])["local_process_modes"] == ["memory-sync", "chat-agent"]
    assert make_plan(["apps/core/src/providers.rs"])["local_process_modes"] == ["chat-agent"]
    assert make_plan(["apps/core/src/store/mailbox.rs"])["local_process_modes"] == ["default", "phase1", "chat-agent", "phase4", "phase5"]
    for path, modes in (
        ("apps/core/src/store/reduce.rs", ["memory-sync", "phase1", "phase5"]),
        ("apps/core/src/store/log.rs", ["memory-sync", "default", "phase1", "phase5"]),
        ("apps/core/src/store/sync.rs", ["memory-sync", "default", "phase4", "phase5"]),
        ("apps/core/src/identity.rs", ["memory-sync", "auto-route", "default", "phase4", "public-node", "phase5"]),
        ("apps/core/src/net.rs", ["auto-route", "default", "phase4", "public-node", "mesh", "delivery-charity"]),
        ("apps/core/src/runtime/peers.rs", ["default", "phase4", "public-node", "mesh", "delivery-charity"]),
        ("apps/core/src/runtime/codex_server.rs", ["chat-agent"]),
        ("crates/secure/src/recovery.rs", ["memory-sync", "auto-route", "default", "phase5"]),
        ("crates/bridge/src/codex.rs", ["chat-agent", "phase4"]),
        ("crates/control/src/lib.rs", ["auto-route", "chat-agent", "phase4", "public-node", "phase5"]),
    ):
        assert make_plan([path])["local_process_modes"] == modes, path
    combined = make_plan(["apps/core/src/store/bot_settings.rs", "apps/core/src/store/mailbox.rs", "apps/core/src/store/bot_settings.rs"])
    assert combined["local_process_modes"] == ["memory-sync", "default", "phase1", "chat-agent", "phase4", "phase5"]
    for path in ("apps/core/src/store.rs", "apps/core/src/model.rs", "apps/core/new-module.rs", "apps/core/src/runtime/new-module.rs", "crates/protocol/src/lib.rs"):
        assert make_plan([path])["local_process_modes"] == ["memory-sync", "auto-route", "default", "phase1", "chat-agent", "phase4", "public-node", "mesh", "phase5"]
    assert make_plan(["Cargo.lock"])["local_process_modes"] == ["memory-sync", "auto-route", "default", "phase1", "chat-agent", "phase4", "public-node", "mesh", "phase5", "delivery-charity"]
    assert make_plan(["apps/core/src/delivery/charity.rs"])["local_process_modes"][-1] == "delivery-charity"
    expected = {
        "node": ["vibeface-core"],
        "bridge": ["vibeface-bridge", "vibeface-core", "vibeface-control", "vibeface"],
        "protocol": ["vibeface-protocol", "vibeface-bridge", "vibeface-core", "vibeface-control", "vibeface"],
        "secure": ["vibeface-secure", "vibeface-core", "vibeface-secure-ffi"],
        "control": ["vibeface-core", "vibeface-control", "vibeface"],
        "cli": ["vibeface"],
        "delivery": ["vibeface-core", "vibeface-delivery"],
    }
    for crate, packages in expected.items():
        source = "apps/core/src/lib.rs" if crate == "node" else "apps/cli/src/lib.rs" if crate == "cli" else f"crates/{crate}/src/lib.rs"
        plan = make_plan([source])
        assert plan["rust_packages"] == packages and plan["test_tiers"] == ["fast"]
        assert plan["package_mode"] == "web" and plan["package_matrix"]["include"] == list(PLATFORMS[:1])
    assert make_plan(["crates/control/src/lib.rs"])["node"]
    assert make_plan(["crates/control/src/lib.rs"])["rust_packages"] == expected["control"]
    for path in ("crates/delivery/Cargo.toml", "crates/delivery/abi/Statement.sol", "crates/delivery/proof/guest/src/main.rs", "crates/delivery/proof/fixtures/manifest.json", "contracts/delivery/DeliveryEscrow.sol", "contracts/delivery/package-lock.json", "tests/fixtures/delivery-probe/Cargo.toml", "tests/fixtures/delivery-probe/Cargo.lock", "tests/fixtures/delivery-probe/src/main.rs", "apps/core/src/net/mesh/record.rs", "apps/core/src/delivery/wallet.rs", "apps/core/src/client.rs", "scripts/check-delivery.py", "scripts/install-delivery-toolchain.py"):
        selected = make_plan([path])
        assert selected["scopes"] == ["delivery"] and selected["delivery"]
        assert selected["rust_packages"] == expected["delivery"] and selected["web"]
        assert "delivery" in selected["required_jobs"]
        assert "package" in selected["required_jobs"] and selected["package_matrix"]["include"][0]["target"] == "x86_64-unknown-linux-gnu"
    assert not docs["delivery"] and not web["delivery"]
    for path in ("Cargo.toml", "Cargo.lock", "apps/core/Cargo.toml", "apps/cli/Cargo.toml", "crates/control/Cargo.toml", ".cargo/config.toml", "rust-toolchain.toml", ".github/workflows/ci.yml", "scripts/ci.py", "new-build-input"):
        assert make_plan([path])["rust_packages"] == list(PACKAGES)
    assert make_plan(["apps/app/src/a.tsx", "apps/core/src/api.rs"])["rust_packages"] == ["vibeface-core"]
    assert make_plan(["apps/cli/src/main.rs"])["rust_packages"] == ["vibeface"]
    assert make_plan(["apps/core/schema.hcl"])["node"]
    assert make_plan(["install.sh"])["node"]
    for path in ("packages/client/src/avatar/derived-faces.json", "packages/client/src/avatar/face.ts"):
        shared = make_plan([path])
        assert shared["scopes"] == ["mobile", "node"] and shared["rust_packages"] == ["vibeface-core"] and shared["web"]
    for event in ("schedule", "workflow_dispatch", "merge_group"):
        assert make_plan(["README.md"], event)["rust_packages"] == list(PACKAGES)
        assert make_plan(["README.md"], event)["mobile"] and make_plan(["README.md"], event)["desktop"]
    assert make_plan(["README.md"], "push")["docs_only"]
    assert make_plan(["apps/app/src/a.tsx"], "push")["rust_packages"] == []
    assert make_plan(["README.md"], "push", ref_type="tag")["test_tiers"] == ["fast", "slow"]
    assert make_plan(["README.md"], "push", ref_type="tag")["package_matrix"]["include"] == list(PLATFORMS)
    assert not make_plan(["apps/core/src/api.rs"])["npm"]
    assert not make_plan(["apps/core/src/api.rs"], channel="stable")["npm"]
    assert not make_plan(["apps/core/src/api.rs"], channel="rc")["npm"]
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
    for source, check in (("apps/desktop/src/main/bounded-read.ts", "desktop-check"), ("apps/mobile/app/_layout.tsx", "mobile-check")):
        selected = make_plan([source])
        selected_needs = {job: {"result": "success" if job in selected["required_jobs"] else "skipped"} for job in JOBS}
        selected_needs["plan"] = {"result": "success"}
        gate(selected_needs, selected)
        for outcome in ("skipped", "failure", "cancelled"):
            rejected = json.loads(json.dumps(selected_needs))
            rejected[check]["result"] = outcome
            expect_failure(lambda: gate(rejected, selected))
        rejected = dict(selected_needs)
        del rejected[check]
        expect_failure(lambda: gate(rejected, selected))
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
    for result in ("failure", "cancelled"):
        broken = json.loads(json.dumps(needs))
        broken["rust-slow"]["result"] = result
        expect_failure(lambda: gate(broken))
    gate({job: value for job, value in needs.items() if job != "rust-slow"})
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
    expect_failure(lambda: gate(complete, desktop=True))
    for job in DESKTOP_JOBS:
        complete[job] = {"result": "success"}
    gate(complete, desktop=True)
    gate(complete)
    for result in ("failure", "cancelled", "skipped", "missing"):
        for job in DESKTOP_JOBS:
            broken = json.loads(json.dumps(complete))
            broken[job] = {"result": result}
            expect_failure(lambda: gate(broken, desktop=True))
            # Without the dispatch input a skipped desktop job is expected, but a failed one still fails the gate.
            if result in {"failure", "cancelled"}:
                expect_failure(lambda: gate(broken))
            else:
                gate(broken)
    expect_failure(lambda: gate(needs, desktop=True))
    for job in DESKTOP_JOBS:
        del complete[job]
    delivery_plan = make_plan(["crates/delivery/proof/fixtures/manifest.json"])
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
        source = Path(directory) / "apps/core/src/old.rs"
        source.parent.mkdir(parents=True)
        source.write_text("test fixture\n")
        (Path(directory) / "README.md").write_text("docs\n")
        (Path(directory) / "README.zh-CN.md").write_text("docs\n")
        git("add", ".", cwd=directory)
        git("-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false", "-c", "user.name=CI self-test", "-c", "user.email=ci@example.invalid", "commit", "--quiet", "-m", "base", cwd=directory)
        base = git("rev-parse", "HEAD", cwd=directory).decode().strip()
        target = Path(directory) / "apps/app/old.rs"
        target.parent.mkdir(parents=True)
        source.rename(target)
        (Path(directory) / "README.md").unlink()
        (Path(directory) / "README.zh-CN.md").unlink()
        git("add", "-A", cwd=directory)
        git("-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false", "-c", "user.name=CI self-test", "-c", "user.email=ci@example.invalid", "commit", "--quiet", "-m", "rename and delete", cwd=directory)
        paths = changed_paths(base, "HEAD", "pull_request", cwd=directory)
        assert set(paths) == {"README.md", "README.zh-CN.md", "apps/core/src/old.rs", "apps/app/old.rs"}
        assert make_plan(paths)["rust_packages"] == ["vibeface-core"]
        assert make_plan(paths)["local_process_modes"] == ["memory-sync", "auto-route", "default", "phase1", "chat-agent", "phase4", "public-node", "mesh", "phase5"]
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
        fixture_git = git
        with patch.dict(os.environ, environment), patch(f"{__name__}.git", side_effect=lambda *args, **kwargs: fixture_git(*args, cwd=directory)), patch(f"{__name__}.changed_paths", return_value=paths) as diff, patch(f"{__name__}.release_metadata", return_value={"channel": "nightly"}):
            for draft in (True, False):
                event_file.write_text(json.dumps({"pull_request": {"base": {"sha": new_base}, "head": {"sha": pr_head}, "draft": draft}}))
                arguments.base = None
                selected = event_plan(arguments)
                diff.assert_called_with(new_base, pr_head, "pull_request")
                assert selected == dict(make_plan(paths), coverage_base=new_base, coverage_head=pr_head, coverage_reason="event-range", baseline_run_id=None)
                assert selected["test_tiers"] == ["fast"]
        coverage_self_test(directory, base, new_base)
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
            run_check(["vibeface", "vibeface-core", "vibeface-control"], "1.89" if check_msrv else None)
            commands = [call.args[0] for call in run.call_args_list]
            assert commands[0] == ["cargo", "clippy", "--all-targets", "--locked", "-p", "vibeface", "-p", "vibeface-core", "-p", "vibeface-control", "--", "-D", "warnings"]
            assert commands[1:] == ([
                ["rustup", "toolchain", "install", "1.89", "--profile", "minimal"],
                ["cargo", "+1.89", "check", "--locked", "-p", "vibeface", "-p", "vibeface-core", "-p", "vibeface-control"],
            ] if check_msrv else [])
    root = Path(__file__).resolve().parent.parent
    workflow = (root / ".github/workflows/ci.yml").read_text()
    ci_gate = workflow.split("\n  ci-gate:\n", 1)[1]
    for job, output in (("mobile-check", "mobile"), ("desktop-check", "desktop")):
        assert f"{output}: ${{{{ steps.select.outputs.{output} }}}}" in workflow
        block = re.search(rf"(?ms)^  {job}:\n(.*?)(?=^  [\w-]+:\n|\Z)", workflow)[1]
        assert "    needs: plan\n" in block and f"needs.plan.outputs.{output} == 'true'" in block
        assert job in ci_gate.split("    runs-on:", 1)[0]
    web_build = (root / "scripts/build-web.sh").read_text()
    assert '--filter "vibeface-mobile..."' in web_build, "Web behavior tests read Mobile package helpers"
    assert "pnpm --filter @vibeface/secure-native run typecheck" not in web_build
    assert "pnpm --filter vibeface-mobile run typecheck" not in web_build
    for name in ("mobile.yml", "release.yml"):
        standalone = (root / ".github/workflows" / name).read_text()
        assert "pnpm --filter @vibeface/secure-native run typecheck" in standalone
        assert "pnpm --filter vibeface-mobile run typecheck" in standalone
    concurrency = workflow.split("concurrency:\n", 1)[1].split("\npermissions:", 1)[0]
    assert "(github.event_name == 'workflow_dispatch' || github.run_attempt > 1) && github.run_id || ''" in concurrency
    assert "cancel-in-progress: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && github.run_attempt == 1 }}" in concurrency
    assert "fetch-depth: 0" in workflow.split("\n  ci-gate:\n", 1)[1]
    for job in ("rust-check", "rust-fast", "rust-slow"):
        block = re.search(rf"(?ms)^  {job}:\n(.*?)(?=^  \w|\Z)", workflow)[1]
        dependency = "[plan, quick-check]" if job == "rust-check" else "plan"
        assert f"    needs: {dependency}\n" in block
        assert "actions/download-artifact@" not in block
    package = re.search(r"(?ms)^  package:\n(.*?)(?=^  \w|\Z)", workflow)[1]
    assert "    needs: [plan, crypto-web, web]\n" in package
    assert "name: crypto-web-${{ github.sha }}" in package and "name: web-${{ github.sha }}" in package
    subprocess.run(["python3", str(root / "scripts/build-inputs.py"), "self-test"], check=True)
    config = (root / "crates/control/src/lib.rs").read_text()
    protocol, epoch = (re.search(rf"^pub const {name}: u32 = (\d+);$", config, re.M)[1] for name in ("UPDATE_PROTOCOL", "DATA_EPOCH"))
    assert f'info["update_protocol"]=="{protocol}" && info["data_epoch"]=="{epoch}"' in (root / "install.sh").read_text(), "install.sh accepts the build's update protocol and data epoch"
    release_privacy_test(root, scratch)
    gatekeeper_test(root, scratch)
    print("CI selector, event, test partition, check commands, rename/delete, gate and installer epoch self-tests passed")


def coverage_self_test(directory, base, nonancestor):
    import io
    from unittest.mock import patch

    event_file = Path(directory) / "event.json"
    event_file.unlink(missing_ok=True)

    def commit(message):
        git("add", "-A", cwd=directory)
        git("-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false", "-c", "user.name=CI self-test", "-c", "user.email=ci@example.invalid", "commit", "--quiet", "-m", message, cwd=directory)
        return git("rev-parse", "HEAD", cwd=directory).decode().strip()

    git("checkout", "--quiet", "--detach", base, cwd=directory)
    (Path(directory) / "apps/core/src/old.rs").write_text("changed Rust input\n")
    rust_head = commit("Rust change A")
    (Path(directory) / "README.zh-CN.md").write_text("docs change B\n")
    head = commit("docs change B")
    event_file.write_text(json.dumps({"before": rust_head, "after": head}))
    environment = {"GITHUB_ACTIONS": "true", "GITHUB_EVENT_NAME": "push", "GITHUB_REF_TYPE": "branch", "GITHUB_REF": "refs/heads/main", "GITHUB_SHA": head, "GITHUB_REPOSITORY": "fixture/vibeface", "GITHUB_RUN_ID": "50", "GH_TOKEN": "fixture-token", "GITHUB_EVENT_PATH": str(event_file)}
    args = argparse.Namespace(event=None, base=None, head=None)
    successful = {"id": 7, "repository": {"full_name": "fixture/vibeface"}, "path": ".github/workflows/ci.yml", "head_branch": "main", "head_sha": base, "event": "push", "status": "completed", "conclusion": "success"}
    fixture_git = git
    with patch.dict(os.environ, environment), patch(f"{__name__}.git", side_effect=lambda *args, **kwargs: fixture_git(*args, cwd=directory)), patch(f"{__name__}.release_metadata", return_value={"channel": "nightly"}):
        runs = [dict(successful, id=8, head_sha=rust_head, conclusion="cancelled"), dict(successful, id=9, head_sha=rust_head, conclusion="skipped"), successful]
        with patch("urllib.request.urlopen", return_value=io.BytesIO(json.dumps({"workflow_runs": runs}).encode())) as query:
            cumulative = event_plan(args)
            assert cumulative["coverage_base"] == base and cumulative["coverage_head"] == head and cumulative["baseline_run_id"] == 7
            assert cumulative["scopes"] == ["docs", "node"] and cumulative["rust_packages"] == ["vibeface-core"]
            assert cumulative["test_tiers"] == ["fast"] and not cumulative["extended"]
            request = query.call_args.args[0]
            assert request.full_url == "https://api.github.com/repos/fixture/vibeface/actions/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=100"
            assert request.get_header("Authorization") == "Bearer fixture-token"
        with patch("urllib.request.urlopen", return_value=io.BytesIO(json.dumps({"workflow_runs": [dict(successful, head_sha=rust_head)]}).encode())):
            narrowed = event_plan(args)
            assert narrowed["coverage_base"] == rust_head and narrowed["scopes"] == ["docs"]
            assert narrowed["required_jobs"] == ["quick-check"] and narrowed["local_process_modes"] == []
        with patch("urllib.request.urlopen", return_value=io.BytesIO(json.dumps({"workflow_runs": [dict(successful, head_sha=head)]}).encode())):
            assert successful_push_base(rust_head) is None
        cases = [[], [dict(successful, head_sha=nonancestor)], [dict(successful, head_sha=head)], [dict(successful, id=50)], [dict(successful, head_sha="bad-sha")], [dict(successful, event="workflow_dispatch")], [dict(successful, head_branch="feature")], [dict(successful, repository={"full_name": "other/repo"})], [dict(successful, path=".github/workflows/release.yml")], [dict(successful, status="in_progress")], [{}], None]
        for runs in cases:
            with patch("urllib.request.urlopen", return_value=io.BytesIO(json.dumps({"workflow_runs": runs}).encode())):
                fallback = event_plan(args)
                assert fallback["coverage_base"] is None and fallback["coverage_reason"] == "no-trusted-main-push"
                assert fallback["rust_packages"] == list(PACKAGES) and fallback["test_tiers"] == ["fast"]
        for response in (OSError("API unavailable"), http.client.IncompleteRead(b""), urllib.error.HTTPError("https://api.github.com", 401, "unauthorized", {}, None), io.BytesIO(b"invalid JSON")):
            with patch("urllib.request.urlopen", **({"side_effect": response} if isinstance(response, Exception) else {"return_value": response})):
                assert event_plan(args)["rust_packages"] == list(PACKAGES)
        with patch.dict(os.environ, {"GH_TOKEN": ""}), patch("urllib.request.urlopen", side_effect=AssertionError("No token must not query the API")):
            assert event_plan(args)["coverage_base"] is None
            assert event_plan(args)["full"]
        needs = {job: {"result": "success" if job in cumulative["required_jobs"] else "skipped"} for job in JOBS}
        needs["plan"] = {"result": "success"}
        with patch("urllib.request.urlopen", side_effect=AssertionError("Gate must not query a later baseline")):
            gate(needs, cumulative)
            for altered in (dict(cumulative, coverage_head=rust_head), dict(cumulative, coverage_base=head), dict(cumulative, coverage_base=nonancestor), dict(cumulative, coverage_base=None), dict(cumulative, scopes=["docs"]), dict(cumulative, required_jobs=["quick-check"]), dict(cumulative, event="workflow_dispatch"), dict(cumulative, local_process_modes=[])):
                expect_failure(lambda: gate(needs, altered))
            gate({"plan": {"result": "success"}, **{job: {"result": "success"} for job in JOBS}}, fallback)
            for event in ("schedule", "workflow_dispatch", "merge_group"):
                with patch.dict(os.environ, {"GITHUB_EVENT_NAME": event}):
                    selected = event_plan(argparse.Namespace(event=None, base=None, head=None))
                    assert selected["full"] and selected["coverage_reason"] == "full-event"
            explicit = event_plan(argparse.Namespace(event="push", base=rust_head, head=head))
            assert explicit["scopes"] == ["docs"] and explicit["coverage_reason"] == "event-range"
    print("Cumulative push coverage, API fallback and frozen gate self-tests passed")


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
        for name in ("scripts/release.py", "apps/core/Cargo.toml", "apps/cli/Cargo.toml", "apps/app/package.json", "apps/mobile/app.json", "apps/desktop/package.json", "scripts/package-node.sh", "scripts/release-rustflags.py", "scripts/build-inputs.py", "scripts/install-node-service.sh", "Cargo.toml", "crates/control/src/lib.rs", "README.md"):
            target = repo / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(root / name, target)
        source = repo / "private build.rs"
        source.write_text('#[cfg(not(privacy_flag))] compile_error!("caller flag lost"); fn main() { println!("{}", file!()); }')
        binary = repo / "target/x86_64-unknown-linux-gnu/release/vibeface"
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
        (repo / "packages/docs").mkdir(parents=True)
        (repo / "packages/docs/guide.md").write_text("public guide")
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
        fake_cargo.write_text('#!/bin/sh\nset -eu\ntouch "$CARGO_MARKER"\nmkdir -p "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release"\nprintf "#!/bin/sh\\nprintf \\\"vibeface 0.0.1\\\\n\\\"\\n" > "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/vibeface"\nprintf "#!/bin/sh\\nprintf \\\"vibeface-core 0.0.1\\\\n\\\"\\n" > "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/vibeface-core"\nchmod +x "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/vibeface" "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/vibeface-core"\n')
        fake_cargo.chmod(0o755)
        marker = repo / "cargo-called"
        environment.update(PATH=f"{fake_cargo.parent}{os.pathsep}{environment['PATH']}", CARGO_MARKER=str(marker))
        subprocess.run(["bash", str(repo / "scripts/package-node.sh"), "--target", "x86_64-unknown-linux-gnu"], env=environment, check=True, stdout=subprocess.PIPE)
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
            assert archive.getmember(f"{package}/vibeface-core").mode & 0o111
        (repo / "scripts/release-rustflags.py").write_text('raise RuntimeError("fixture failure")')
        generated()
        marker.unlink()
        assert subprocess.run(["bash", str(repo / "scripts/package-node.sh"), "--target", "x86_64-unknown-linux-gnu"], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE).returncode != 0
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
    final_gate.add_argument("--desktop", action="store_true", help="also require the explicitly dispatched Windows native and desktop jobs")
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
        from unittest.mock import patch
        with patch.dict(os.environ, {key: value for key, value in os.environ.items() if not key.startswith("GITHUB_") and key != "GH_TOKEN"}, clear=True):
            self_test()
    elif args.command == "plan":
        write_plan(event_plan(args), args.github_output)
    elif args.command == "gate":
        needs = json.loads(args.needs.read_text() if args.needs else os.environ[args.needs_env])
        gate(needs, json.loads(args.plan.read_text()) if args.plan else None, delivery_live=args.delivery_live,
             desktop=args.desktop)
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
