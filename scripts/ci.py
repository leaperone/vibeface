#!/usr/bin/env python3
"""Select CI coverage once, run its Rust test tiers, and check the final gate."""

import sys
sys.dont_write_bytecode = True
try:
    import ci_registry
except ModuleNotFoundError:
    # The release CLI contract runs a minimal copied `ci.py --help` fixture.
    # It does not need the registry, and the normal repository entry point has it.
    if "--help" in sys.argv:
        raise SystemExit(0)
    raise
import release_source

import argparse
import datetime
import fnmatch
import http.client
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.request


REGISTRY = ci_registry.load_registry()

# Keep the public ordering consumed by workflow outputs and timing summaries.
RUST_TARGETS = REGISTRY["rust"]
CRATES = {item["id"]: item["package"] for item in RUST_TARGETS}
DEPENDENTS = {item["id"]: tuple(item["dependents"]) for item in RUST_TARGETS}
PACKAGES = tuple(item["package"] for item in RUST_TARGETS)

RELEASE_TOOL_PATHS = REGISTRY["paths"]["release_tools"]
NODE_PROCESS_PATTERNS = tuple(
    pattern
    for rule in REGISTRY["primary"]
    if "node" in rule["scopes"] and "paths" in rule
    for pattern in rule["patterns"]
)
SLOW_TESTS = (
    "runtime::peers::tests::a_round_left_behind_pulls_again_without_waiting_for_hello",
)
PLATFORMS = (
    {"runner": "ubuntu-24.04", "target": "x86_64-unknown-linux-gnu"},
    {"runner": "ubuntu-24.04-arm", "target": "aarch64-unknown-linux-gnu"},
    {"runner": "macos-15", "target": "aarch64-apple-darwin"},
    {"runner": "windows-2025", "target": "x86_64-pc-windows-msvc"},
)
JOBS = ("quick-check", "crypto-web", "web", "web-tests", "mobile-check", "desktop-check", "rust-check", "rust-fast", "rust-slow", "delivery", "package", "npm-package", "npm-smoke", "www")
LOCAL_PROCESS_RULES = tuple((item["id"], item["patterns"]) for item in REGISTRY["process"])
ORDINARY_PROCESS_MODES = {item["id"] for item in REGISTRY["process"] if item.get("ordinary", True)}


def primary_scopes(path):
    for rule in REGISTRY["primary"]:
        matched = False
        if "exact" in rule:
            matched = path in rule["exact"]
        elif "regex" in rule:
            matched = rule["regex"].fullmatch(path) is not None
        elif "prefix" in rule:
            suffixes = tuple(rule.get("suffix", ()))
            matched = path.startswith(rule["prefix"]) and (not suffixes or path.endswith(suffixes))
        else:
            matched = any(fnmatch.fnmatchcase(path, pattern) for pattern in rule["patterns"])
        if matched:
            return tuple(rule["scopes"])
    return ("full",)


def local_process_modes(paths, full):
    ordinary = ORDINARY_PROCESS_MODES
    selected = {mode for mode, _ in LOCAL_PROCESS_RULES} if full else set()
    for path in paths:
        matched = {mode for mode, patterns in LOCAL_PROCESS_RULES if any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns)}
        selected.update(matched)
        if not matched & ordinary and (path.startswith(("apps/core/", "apps/cli/", "crates/")) or any(fnmatch.fnmatchcase(path, pattern) for pattern in NODE_PROCESS_PATTERNS)):
            selected.update(ordinary)
    return [mode for mode, _ in LOCAL_PROCESS_RULES if mode in selected]


def make_plan(paths, event="pull_request", ref_type="branch", channel="hourly", *, package_policy="standard"):
    scopes = set()
    for path in paths:
        scopes.update(primary_scopes(path))
        for rule in REGISTRY["overlay"]:
            if any(fnmatch.fnmatchcase(path, pattern) for pattern in rule["patterns"]):
                scopes.update(rule["scopes"])
    plan = scope_plan(scopes, event, ref_type, channel, package_policy=package_policy)
    return dict(plan, changed_files=len(paths), local_process_modes=local_process_modes(paths, plan["full"]))


def scope_plan(scopes, event, ref_type, channel, *, package_policy="standard"):
    if (
        scopes - REGISTRY["scopes"]
        or event not in {"pull_request", "push", "schedule", "workflow_dispatch", "merge_group"}
        or ref_type not in {"branch", "tag"}
        or channel not in {"stable", "rc", "hourly"}
        or not isinstance(package_policy, str) or package_policy not in {"standard", "linux-native"}
        or package_policy == "linux-native" and event != "workflow_dispatch"
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
    web_tests = "web-tests" in scopes and not web
    crypto = web or web_tests
    www = full or "www" in scopes
    mobile = full or "mobile" in scopes
    desktop = full or "desktop" in scopes or bool(affected & {"client"})
    slow = "node" in affected and extended
    delivery = full or "delivery" in affected or "delivery-acceptance" in scopes
    platforms = PLATFORMS if "node" in affected and extended and package_policy == "standard" else PLATFORMS[:1]
    plan = {
        "full": full,
        "extended": extended,
        "check_msrv": bool(packages) and extended,
        "docs_only": not crypto and not packages and not mobile and not desktop and "release-tools" not in scopes,
        "event": event,
        "package_policy": package_policy,
        "ref_type": ref_type,
        "channel": channel,
        "scopes": sorted(scopes),
        "web": web,
        "web_tests": web_tests,
        "crypto": crypto,
        "www": www,
        "mobile": mobile,
        "desktop": desktop,
        "node": "node" in affected,
        "delivery": delivery,
        "rust_packages": packages,
        "test_tiers": (["fast"] if packages else []) + (["slow"] if slow else []),
        "package_mode": "node" if "node" in affected and extended else "web",
        "package_matrix": {"include": list(platforms) if web else []},
        "npm": channel == "hourly" and "node" in affected and extended and package_policy == "standard",
    }
    required = {"quick-check"}
    if crypto:
        required.add("crypto-web")
    if web:
        required.update(("web", "package"))
    if web_tests:
        required.add("web-tests")
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


def package_policy(value, event):
    if not isinstance(value, str) or value not in {"standard", "linux-native"}:
        raise ValueError("Package policy must be standard or linux-native")
    if value == "linux-native" and event != "workflow_dispatch":
        raise ValueError("Linux native package coverage requires explicit workflow_dispatch")
    return value


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
    plan = make_plan(paths, event, ref_type, metadata["channel"], package_policy=package_policy(getattr(args, "package_policy", "standard"), event))
    plan.update(metadata)
    plan.update(coverage_base=base, coverage_head=head, coverage_reason=reason, baseline_run_id=baseline_run)
    return plan


def release_metadata(ref_type, ref_name):
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"], text=True))
    package = next(package for package in metadata["packages"] if package["name"] == "vibeface")
    version = package["version"]
    number = r"(?:0|[1-9][0-9]*)"
    match = re.fullmatch(rf"{number}\.{number}\.{number}(?:-(hourly)\.([0-9]{{10}})|-(rc)\.([1-9][0-9]*))?", version)
    if not match:
        raise ValueError("Expected X.Y.Z, X.Y.Z-hourly.YYYYMMDDHH or X.Y.Z-rc.N")
    channel = "hourly" if match[1] else "rc" if match[3] else "stable"
    if channel == "hourly":
        stamp = match[2]
        datetime.datetime(int(stamp[:4]), int(stamp[4:6]), int(stamp[6:8]), int(stamp[8:]))
    if ref_type == "tag":
        if ref_name != f"cli-v{version}":
            raise ValueError("Release tag must exactly match the Cargo version")
        release_source.validate_source(git("rev-parse", "HEAD").decode().strip(), "cli")
    return {"version": version, "msrv": package["rust_version"], "channel": channel, "prerelease": channel != "stable"}


def write_plan(plan, output=None):
    encoded = json.dumps(plan, separators=(",", ":"))
    print(encoded)
    if output:
        outputs = {
            "plan": encoded,
            "web": str(plan["web"]).lower(),
            "web_tests": str(plan["web_tests"]).lower(),
            "crypto": str(plan["crypto"]).lower(),
            "www": str(plan["www"]).lower(),
            "mobile": str(plan["mobile"]).lower(),
            "desktop": str(plan["desktop"]).lower(),
            "rust": str(bool(plan["rust_packages"])).lower(),
            "slow": str("slow" in plan["test_tiers"]).lower(),
            "delivery": str(plan["delivery"]).lower(),
            "source_sha": plan["coverage_head"],
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
                f"- Package targets: {', '.join(row['target'] for row in plan['package_matrix']['include']) or 'none'}\n"
                f"- Package policy: {plan['package_policy']}\n"
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


def frozen_plan(plan, policy="standard"):
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
    return make_plan(paths, plan["event"], plan["ref_type"], plan["channel"], package_policy=package_policy(policy, plan["event"]))


def gate(needs, plan=None, policy="standard"):
    if needs.get("plan", {}).get("result") != "success":
        raise ValueError("CI plan did not succeed")
    if plan is None:
        plan = json.loads(needs["plan"]["outputs"]["plan"])
    if "coverage_head" in plan or os.environ.get("GITHUB_ACTIONS") == "true":
        try:
            expected = frozen_plan(plan, policy)
        except (KeyError, TypeError, subprocess.CalledProcessError) as error:
            raise ValueError("Invalid frozen CI coverage") from error
    else:
        expected = scope_plan(set(plan["scopes"]), plan["event"], plan["ref_type"], plan["channel"], package_policy=package_policy(policy, plan["event"]))
    if any(plan.get(key) != value for key, value in expected.items()):
        raise ValueError("Required coverage differs from the selected CI scope and scenario")
    required = expected["required_jobs"]
    failed = [job for job in required if needs.get(job, {}).get("result") != "success"]
    failed.extend(job for job in JOBS
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


def timed_cargo(command, phase, phases, **kwargs):
    started = time.perf_counter()
    code = None
    try:
        result = subprocess.run(command, check=True, **kwargs)
        code = result.returncode
        return result
    except subprocess.CalledProcessError as error:
        code = error.returncode
        raise
    finally:
        phases.append({"phase": phase, "seconds": round(time.perf_counter() - started, 6), "exit_code": code})


def run_test(packages, tier, timings_file=None):
    selected = package_args(packages)
    test_package = "vibeface-core" if "vibeface-core" in packages else None
    if tier == "slow" and not test_package:
        raise ValueError("Slow tier requires the Core crate")
    compile_args = selected if tier == "fast" else ["-p", test_package, "--lib"]
    if tier == "slow":
        command = ["cargo", "test", "--locked", "-p", test_package, "--lib", "--", "--exact", "--include-ignored", *SLOW_TESTS]
    else:
        skips = [argument for name in SLOW_TESTS for argument in ("--skip", name)]
        command = ["cargo", "test", "--locked", "--no-fail-fast", *selected, "--", *skips]
    phases = []
    try:
        timed_cargo(["cargo", "test", "--locked", "--no-run", *compile_args], "compile", phases)
        if test_package:
            listing = timed_cargo(["cargo", "test", "--locked", *compile_args, "--", "--list"], "enumerate", phases, capture_output=True, text=True).stdout
            names = [line[:-6] for line in listing.splitlines() if line.endswith(": test")]
            fast = validate_slow_tests(names)
            print(f"Partition: {len(fast)} fast test names, {len(SLOW_TESTS)} exact slow tests", flush=True)
        timed_cargo(command, "execute", phases)
    finally:
        report = {"tier": tier, "packages": packages, "status": "failure" if sys.exc_info()[0] else "success", "phases": phases}
        print("Rust test timing: " + json.dumps(report), flush=True)
        if timings_file:
            failed = sys.exc_info()[0] is not None
            try:
                timings_file.parent.mkdir(parents=True, exist_ok=True)
                timings_file.write_text(json.dumps(report, indent=2) + "\n")
                summary = os.environ.get("GITHUB_STEP_SUMMARY")
                if summary:
                    with open(summary, "a") as destination:
                        destination.write(f"\n### Rust {tier} timing\n\n| Phase | Seconds | Exit code |\n| --- | ---: | ---: |\n")
                        for row in phases:
                            destination.write(f"| {row['phase']} | {row['seconds']:.2f} | {row['exit_code']} |\n")
            except OSError:
                if not failed:
                    raise
                print("Could not write Rust timing report; preserving the test failure", file=sys.stderr)


def run_check(packages, msrv=None):
    selected = package_args(packages)
    subprocess.run(["cargo", "clippy", "--all-targets", "--locked", *selected, "--", "-D", "warnings"], check=True)
    if msrv:
        subprocess.run(["rustup", "toolchain", "install", msrv, "--profile", "minimal"], check=True)
        subprocess.run(["cargo", f"+{msrv}", "check", "--locked", *selected], check=True)


def self_test():
    if __name__ == "__main__":
        sys.modules["ci"] = sys.modules[__name__]
    from tests.ci.test_cases import run_all

    run_all()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    commands = parser.add_subparsers(dest="command")
    plan = commands.add_parser("plan")
    plan.add_argument("--base")
    plan.add_argument("--head")
    plan.add_argument("--event", choices=("pull_request", "push", "schedule", "workflow_dispatch", "merge_group"))
    plan.add_argument("--github-output")
    plan.add_argument("--package-policy-env", help="internal standard or linux-native policy environment variable")
    final_gate = commands.add_parser("gate")
    final_gate.add_argument("--plan", type=Path)
    final_gate.add_argument("--package-policy-env", help="independently validate the internal package policy")
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
            command.add_argument("--timings-file", type=Path, help="write compile, enumeration and execution durations")
        else:
            command.add_argument("--msrv", help="also check the declared minimum Rust version")
    args = parser.parse_args()
    if args.self_test:
        from unittest.mock import patch
        with patch.dict(os.environ, {key: value for key, value in os.environ.items() if not key.startswith("GITHUB_") and key != "GH_TOKEN"}, clear=True):
            self_test()
    elif args.command == "plan":
        args.package_policy = os.environ[args.package_policy_env] if args.package_policy_env else "standard"
        write_plan(event_plan(args), args.github_output)
    elif args.command == "gate":
        needs = json.loads(args.needs.read_text() if args.needs else os.environ[args.needs_env])
        gate(needs, json.loads(args.plan.read_text()) if args.plan else None,
             policy=os.environ[args.package_policy_env] if args.package_policy_env else "standard")
    elif args.command in {"test", "check"}:
        packages = args.packages if args.packages else json.loads(os.environ[args.plan_env])["rust_packages"]
        if args.command == "test":
            run_test(packages, args.tier, args.timings_file)
        else:
            plan = json.loads(os.environ[args.plan_env]) if args.plan_env else None
            if plan and plan["check_msrv"] and not args.msrv:
                parser.error("The selected CI plan requires --msrv")
            run_check(packages, args.msrv if plan is None or plan["check_msrv"] else None)
    else:
        parser.error("Select a command or --self-test")


if __name__ == "__main__":
    main()
