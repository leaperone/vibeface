"""Planner, gate and Cargo command cases."""

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

from ci import (
    JOBS,
    LOCAL_PROCESS_RULES,
    PACKAGES,
    PLATFORMS,
    REGISTRY,
    RELEASE_TOOL_PATHS,
    SLOW_TESTS,
    gate,
    make_plan,
    package_args,
    run_check,
    run_test,
    validate_slow_tests,
)
from .support import ROOT, expect_failure


def focused_self_test():
    from unittest.mock import patch
    root = ROOT
    unit_jobs = ["quick-check", "crypto-web", "web-tests"]
    for path in [*root.glob("apps/app/src/lib/*.test.ts"), *root.glob("apps/app/src/features/*/*.test.ts")]:
        plan = make_plan([path.relative_to(root).as_posix()])
        assert plan["required_jobs"] == unit_jobs and not plan["docs_only"]
    for path in ("tests/e2e/onboarding.e2e.ts", "tests/e2e/fixtures.ts", "tests/e2e/e2e.native.config.ts"):
        assert make_plan([path])["docs_only"], path
    for path in ("apps/app/src/lib/nested/new.test.ts", "apps/app/src/lib/new.test.tsx"):
        assert make_plan([path])["full"], path
    assert make_plan(["apps/app/src/lib/markdown.test.ts", "apps/core/src/api.rs"])["rust_packages"] == ["vibeface-core"]
    assert "web-tests" not in make_plan(["apps/app/src/lib/markdown.test.ts", "apps/app/src/lib/markdown.ts"])["required_jobs"]
    assert make_plan(["scripts/check-web.sh"])["web"], "The shared check entry must select the Web build"
    plan = make_plan(["apps/app/src/lib/markdown.test.ts"])
    needs = {job: {"result": "success" if job in unit_jobs else "skipped"} for job in JOBS}
    needs["plan"] = {"result": "success"}
    gate(needs, plan)
    for result in ("skipped", "failure", "cancelled", "missing"):
        broken = json.loads(json.dumps(needs))
        if result == "missing":
            del broken["web-tests"]
        else:
            broken["web-tests"]["result"] = result
        expect_failure(lambda: gate(broken, plan))
    expect_failure(lambda: gate(needs, dict(plan, crypto=False)))
    workflow = (root / ".github/workflows/ci.yml").read_text()
    block = re.search(r"(?ms)^  web-tests:\n(.*?)(?=^  [\w-]+:\n|\Z)", workflow)[1]
    assert "needs: [plan, crypto-web]" in block and "needs.plan.outputs.web_tests == 'true'" in block
    for required in ("name: crypto-web-${{ github.sha }}", "check crypto", "'vibeface-mobile...'", "bash scripts/check-web.sh"):
        assert required in block, required
    assert "web-tests" in workflow.split("\n  ci-gate:\n", 1)[1].split("runs-on:", 1)[0]
    package = re.search(r"(?ms)^  package:\n(.*?)(?=^  [\w-]+:\n|\Z)", workflow)[1]
    assert "key: package-with-probe-${{ matrix.target }}-${{ hashFiles('scripts/release-rustflags.py') }}" in package
    assert "tests/fixtures/delivery-probe -> ../../../.artifacts/delivery-probe/target" in package
    fixture = root / "tests/fixtures/delivery-probe"
    assert (fixture / "../../../.artifacts/delivery-probe/target").resolve() == root / ".artifacts/delivery-probe/target"
    listing = "other::test: test\n" + "\n".join(name + ": test" for name in SLOW_TESTS)
    def cargo(command, **kwargs):
        return subprocess.CompletedProcess(command, 0, listing if "--list" in command else "")
    with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}):
        output = Path(directory) / "timing.json"
        with patch("subprocess.run", side_effect=cargo) as calls:
            run_test(["vibeface-core"], "fast", output)
        report = json.loads(output.read_text())
        assert [row["phase"] for row in report["phases"]] == ["compile", "enumerate", "execute"]
        assert report["status"] == "success" and all(row["exit_code"] == 0 and row["seconds"] >= 0 for row in report["phases"])
        assert calls.call_args_list[0].args[0] == ["cargo", "test", "--locked", "--no-run", "-p", "vibeface-core"]
        assert "--skip" in calls.call_args_list[-1].args[0] and "--no-fail-fast" in calls.call_args_list[-1].args[0]
        with patch("subprocess.run", side_effect=cargo) as calls:
            run_test(["vibeface-core"], "slow", output)
        assert calls.call_args.args[0] == ["cargo", "test", "--locked", "-p", "vibeface-core", "--lib", "--", "--exact", "--include-ignored", "runtime::peers::tests::a_round_left_behind_pulls_again_without_waiting_for_hello"]
        def failure(command, **kwargs):
            if "--no-fail-fast" in command:
                raise subprocess.CalledProcessError(7, command)
            return cargo(command, **kwargs)
        with patch("subprocess.run", side_effect=failure):
            try:
                run_test(["vibeface-core"], "fast", output)
            except subprocess.CalledProcessError as error:
                assert error.returncode == 7
            else:
                raise AssertionError("test failure was hidden by timing collection")
        failed = json.loads(output.read_text())
        assert failed["status"] == "failure" and failed["phases"][-1]["exit_code"] == 7
        with patch("subprocess.run", side_effect=subprocess.CalledProcessError(8, ["cargo"])) as calls:
            try:
                run_test(["vibeface-core"], "fast", output)
            except subprocess.CalledProcessError as error:
                assert error.returncode == 8
            else:
                raise AssertionError("compile failure was hidden by timing collection")
        failed = json.loads(output.read_text())
        assert calls.call_count == 1 and failed["status"] == "failure" and [row["phase"] for row in failed["phases"]] == ["compile"]
    print("Unit-only coverage, cache wiring and Rust timing failure checks passed")

def planner_matrix():
    from unittest.mock import patch
    focused_self_test()
    with patch.dict(REGISTRY, {"overlay": [{"patterns": ("README.md",), "scopes": ("desktop",)}]}):
        assert make_plan(["README.md"])["scopes"] == ["desktop", "docs"]
    web = make_plan(["apps/app/src/routes/index.tsx"])
    assert web["rust_packages"] == [] and web["test_tiers"] == []
    assert web["required_jobs"] == ["quick-check", "crypto-web", "web", "package"]
    assert web["package_mode"] == "web" and web["package_matrix"]["include"] == list(PLATFORMS[:1])
    assert make_plan(["apps/app/src/content.md"])["web"]
    assert web["local_process_modes"] == []
    for path in ("scripts/run-delivery-live.py", "scripts/verify-delivery-protected.py", "scripts/verify-delivery-restart-observer.py"):
        selected = make_plan([path])
        assert "delivery" in selected["scopes"] and {"quick-check","delivery"}.issubset(selected["required_jobs"])
        assert "delivery-charity" in selected["local_process_modes"]
    for path in (
        "crates/protocol/src/lib.rs", "crates/protocol/src/delivery.rs",
        "crates/protocol/src/delivery/request.rs", "crates/protocol/src/delivery/schema.json",
    ):
        selected = make_plan([path])
        assert selected["scopes"] == ["delivery-acceptance", "protocol"]
        assert selected["delivery"] and "delivery" in selected["required_jobs"]
        assert selected["local_process_modes"] == list(dict(LOCAL_PROCESS_RULES))
        assert selected["rust_packages"] == ["vibeface-protocol", "vibeface-bridge", "vibeface-client",
                                             "vibeface-core", "vibeface-control", "vibeface", "vibeface-secure-ffi", "vibeface-native-transport"]
    bootstrap = make_plan(["config/public-bootstrap.json"])
    assert bootstrap["rust_packages"] == ["vibeface-core"] and bootstrap["web"]
    assert bootstrap["local_process_modes"] == ["auto-route", "default", "public-node", "mesh"]
    www = make_plan(["apps/www/src/routes/index.tsx"])
    assert www["www"] and not www["web"] and not www["rust_packages"]
    assert www["required_jobs"] == ["quick-check", "www"]
    assert make_plan(["packages/client/src/lib/transport.ts", "pnpm-lock.yaml"])["scopes"] == ["desktop", "mobile", "web", "www"]
    assert make_plan(["apps/mobile/app/_layout.tsx", "packages/theme/theme.css"])["scopes"] == ["mobile", "web", "www"]
    for path in ("apps/desktop/src/host/lifecycle.rs", "apps/desktop/tests/native_host.rs"):
        desktop = make_plan([path], "push")
        assert desktop["scopes"] == ["desktop"] and not desktop["full"]
        assert desktop["required_jobs"] == ["quick-check", "desktop-check"]
        assert not desktop["rust_packages"] and not desktop["web"] and not desktop["delivery"]
        assert not desktop["docs_only"] and not desktop["local_process_modes"]
    for path in ("apps/mobile/app/_layout.tsx", "packages/secure-native/src/index.ts"):
        mobile = make_plan([path])
        assert mobile["scopes"] == ["mobile"] and mobile["web"]
        assert mobile["required_jobs"] == ["quick-check", "crypto-web", "web", "mobile-check", "package"]
        assert not mobile["rust_packages"] and not mobile["docs_only"]
    for path in ("packages/client/src/lib/transport.ts", "packages/client/src/avatar/face.ts", "packages/client/src/avatar/derived-faces.json", "packages/theme/theme.css", "package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml"):
        assert make_plan([path])["mobile"], path
    assert make_plan(["crates/secure-ffi/src/lib.rs"])["rust_packages"] == ["vibeface-secure-ffi"]
    assert make_plan(["crates/native-transport/src/lib.rs"])["rust_packages"] == ["vibeface-client", "vibeface-core", "vibeface", "vibeface-secure-ffi", "vibeface-native-transport"]
    docs = make_plan(["README.md", "packages/docs/node.md"])
    assert docs["docs_only"] and docs["www"] and docs["required_jobs"] == ["quick-check", "www"]
    for path in ("README.md", "README.zh-CN.md"):
        assert make_plan([path])["required_jobs"] == ["quick-check"]
    assert make_plan(["README.zh-CN.md", "apps/core/src/api.rs"])["rust_packages"] == ["vibeface-core"]
    for path in ("README.zh-CN.md.rs", ".agents/skills/release/SKILL.md", "scripts/release.py", "scripts/hourly.py",
                 'scripts/release_component_identity.py', 'scripts/release_format.py', 'scripts/release_context.py', 'scripts/release_source.py', 'scripts/release_archive.py', 'scripts/release_inventory.py', 'scripts/release_github.py', 'scripts/release_publisher.py', 'scripts/release_assembly.py', "scripts/tests/release/test_cli.py", "scripts/tests/release/test_publisher.py", "scripts/tests/release/test_source.py",
                 "apps/desktop/build/installer.nsi"):
        assert make_plan([path])["full"]
    for path in RELEASE_TOOL_PATHS:
        selected = make_plan([path])
        assert selected["required_jobs"] == ["quick-check"] and not selected["full"] and not selected["docs_only"]
        assert not selected["desktop"] and not selected["web"] and not selected["local_process_modes"]
    # Linux import fixtures do not exercise Windows ACL/signing: keep the original full gates.
    required_full = ["quick-check", "crypto-web", "web", "mobile-check", "desktop-check",
                     "rust-check", "rust-fast", "delivery", "package", "www"]
    for path in ("scripts/windows-signing.mjs", "scripts/private-control.mjs"):
        selected = make_plan([path])
        assert selected["full"] and not selected["docs_only"] and selected["required_jobs"] == required_full
    # A quick-only Node path does not imply the complete AUTO19 change is quick-only.
    auto19_paths = [*(path for path in RELEASE_TOOL_PATHS if path != "scripts/sealed-runtime.mjs"), ".github/workflows/ci.yml", ".github/workflows/npm-publish.yml",
                    ".preflight.toml", "packages/docs/design/release-modules.md", "scripts/ci.py",
                    "scripts/hourly.py", "scripts/test_release_trains.py", "scripts/check-release-workflow.py"]
    assert len(auto19_paths) == 21
    selected = make_plan(auto19_paths)
    assert selected["full"] and not selected["docs_only"] and selected["required_jobs"] == required_full
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
    for path in ("apps/core/src/store.rs", "apps/core/src/model.rs", "apps/core/new-module.rs", "apps/core/src/runtime/new-module.rs"):
        assert make_plan([path])["local_process_modes"] == ["memory-sync", "auto-route", "default", "phase1", "chat-agent", "phase4", "public-node", "mesh", "phase5"]
    assert make_plan(["Cargo.lock"])["local_process_modes"] == ["memory-sync", "auto-route", "default", "phase1", "chat-agent", "phase4", "public-node", "mesh", "phase5", "delivery-charity"]
    assert make_plan(["apps/core/src/delivery/charity.rs"])["local_process_modes"][-1] == "delivery-charity"
    expected = {
        "node": ["vibeface-core"],
        "bridge": ["vibeface-bridge", "vibeface-client", "vibeface-core", "vibeface-control", "vibeface"],
        "protocol": ["vibeface-protocol", "vibeface-bridge", "vibeface-client", "vibeface-core", "vibeface-control", "vibeface", "vibeface-secure-ffi", "vibeface-native-transport"],
        "secure": ["vibeface-secure", "vibeface-client", "vibeface-core", "vibeface-control", "vibeface", "vibeface-secure-ffi", "vibeface-native-transport"],
        "client": ["vibeface-client", "vibeface-core", "vibeface"],
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
    for provider in ("pi", "opencode"):
        path = f"apps/core/extensions/generated/{provider}.ts"
        assert make_plan([path])["rust_packages"] == ["vibeface-core", "vibeface"]
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
    for source, check in (("apps/desktop/src/host/lifecycle.rs", "desktop-check"), ("apps/mobile/app/_layout.tsx", "mobile-check")):
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
    native = make_plan([], "workflow_dispatch", package_policy="linux-native")
    assert native["package_matrix"]["include"] == list(PLATFORMS[:1]) and native["package_mode"] == "node"
    assert not native["npm"] and native["test_tiers"] == ready["test_tiers"] and native["check_msrv"]
    assert native["required_jobs"] == [job for job in ready["required_jobs"] if job not in ("npm-package", "npm-smoke")]
    native_needs = {job: {"result": "success" if job in native["required_jobs"] else "skipped"} for job in JOBS}
    native_needs["plan"] = {"result": "success", "outputs": {"plan": json.dumps(native)}}
    gate(native_needs, policy="linux-native")
    expect_failure(lambda: gate(native_needs))
    expect_failure(lambda: gate(complete, policy="linux-native"))
    for result in ("failure", "cancelled", "skipped", "missing"):
        for job in ("plan", *native["required_jobs"]):
            broken = json.loads(json.dumps(native_needs))
            if result == "missing":
                del broken[job]
            else:
                broken[job] = {"result": result}
            expect_failure(lambda: gate(broken, policy="linux-native"))
    for job in ("npm-package", "npm-smoke"):
        for result in ("failure", "cancelled"):
            broken = json.loads(json.dumps(native_needs))
            broken[job] = {"result": result}
            expect_failure(lambda: gate(broken, policy="linux-native"))
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

def tool_commands():
    from unittest.mock import patch

    for check_msrv in (False, True):
        with patch("subprocess.run") as run:
            run_check(["vibeface", "vibeface-core", "vibeface-control"], "1.89" if check_msrv else None)
            commands = [call.args[0] for call in run.call_args_list]
            assert commands[0] == ["cargo", "clippy", "--all-targets", "--locked", "-p", "vibeface", "-p", "vibeface-core", "-p", "vibeface-control", "--", "-D", "warnings"]
            assert commands[1:] == ([
                ["rustup", "toolchain", "install", "1.89", "--profile", "minimal"],
                ["cargo", "+1.89", "check", "--locked", "-p", "vibeface", "-p", "vibeface-core", "-p", "vibeface-control"],
            ] if check_msrv else [])
