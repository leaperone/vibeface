"""Workflow inventory, concurrency and source binding cases."""

import re

from ci import JOBS
from .support import ROOT


def concurrency_self_test(workflows):
    from types import SimpleNamespace

    def evaluate(text, filename, inputs, event="workflow_dispatch", run_id=101, attempt=1, ref="refs/heads/main"):
        context = {
            "github": SimpleNamespace(workflow=filename, event_name=event, ref=ref, ref_name=ref.removeprefix("refs/heads/"), run_id=run_id, run_attempt=attempt),
            "inputs": SimpleNamespace(**{"package_policy": "", "intent": "", "platforms": "", "android_only": False, **inputs}),
            "format": lambda template, *values: template.format(*values),
        }

        def expression(source):
            return eval(source.replace("&&", " and ").replace("||", " or "), {"__builtins__": {}}, context)

        if text in {"true", "false"}:
            return text == "true"
        match = re.fullmatch(r"\$\{\{ (.*?) \}\}", text)
        if match:
            return expression(match[1])
        return re.sub(r"\$\{\{ (.*?) \}\}", lambda match: str(expression(match[1])), text)

    selections = {
        "ci.yml": [{"package_policy": "standard"}],
    }
    for filename, modes in selections.items():
        concurrency = workflows[filename].split("concurrency:\n", 1)[1].split("\npermissions:", 1)[0]
        group = re.search(r"^  group: (.+)$", concurrency, re.M)[1]
        cancellation = re.search(r"^  cancel-in-progress: (.+)$", concurrency, re.M)[1]
        assert "github.run_attempt > 1 && format('-rerun-{0}', github.run_id) || ''" in group
        first_groups = []
        for inputs in modes:
            first = evaluate(group, filename, inputs, run_id=100)
            newer = evaluate(group, filename, inputs, run_id=101)
            assert first == newer, (filename, inputs, "first attempts deduplicate")
            assert evaluate(cancellation, filename, inputs) is True
            first_groups.append(first)
            for attempt in (2, 3):
                old_rerun = evaluate(group, filename, inputs, run_id=100, attempt=attempt)
                assert old_rerun == first + "-rerun-100" and old_rerun != newer
                assert evaluate(cancellation, filename, inputs, run_id=100, attempt=attempt) is False
            assert evaluate(group, filename, inputs, ref="refs/heads/other") != first
        assert len(set(first_groups)) == len(modes), (filename, "different modes remain independent")
        if filename == "ci.yml":
            for event in ("push", "schedule", "merge_group"):
                for attempt in (1, 2):
                    assert evaluate(cancellation, filename, modes[0], event=event, attempt=attempt) is (event == "push" and attempt == 1)
                assert evaluate(group, filename, modes[0], event=event, run_id=100, attempt=2) != evaluate(group, filename, modes[0], event=event, run_id=101)
    print("Concurrency controls passed: first-attempt deduplication, isolated old reruns and non-cancelling Core schedules/merge groups")

def workflow_contract_self_test():
    root = ROOT
    directory = root / ".github/workflows"
    workflows = {path.name: path.read_text() for path in directory.glob("*.yml")}
    core = workflows["ci.yml"]

    def block(workflow, job):
        return re.search(rf"(?ms)^  {re.escape(job)}:\n(.*?)(?=^  [\w-]+:\n|\Z)", workflows[workflow].split("\njobs:\n", 1)[1])[1]

    def mapping_keys(text, indentation):
        keys = []
        for line in text.splitlines():
            if not line.startswith(" " * indentation) or len(line) <= indentation or line[indentation].isspace() or line[indentation] == "#":
                continue
            entry = re.fullmatch(r"([^:]+):(?:[ \t].*)?", line[indentation:].rstrip())
            assert entry is not None, "Workflow mapping declarations must use canonical keys"
            keys.append(entry[1].strip())
        return keys

    inventory = {
        "ci.yml": ["plan", *JOBS, "ci-gate"],
    }
    for filename, jobs in inventory.items():
        selected = workflows[filename]
        actual = re.findall(r"(?m)^  ([\w-]+):$", selected.split("\njobs:\n", 1)[1])
        assert set(actual) == set(jobs), (filename, actual, jobs)
        assert "run-name:" in selected
    concurrency_self_test(workflows)
    assert "  schedule:" not in core and "  hourly:" not in core
    manual = core.split("  workflow_dispatch:\n", 1)[1].split("  workflow_call:", 1)[0]
    assert not manual.strip(), "Core manual dispatch has no cross-purpose switches"
    for retired in ("delivery_live", "delivery_small", "windows_only", "desktop_only", "mobile_apps", "inputs.desktop"):
        assert retired not in core
    assert "name: ci-gate\n    if: always()" in block("ci.yml", "ci-gate")
    assert core.count("CI_PACKAGE_POLICY: ${{ inputs.package_policy || 'standard' }}") == 2
    assert core.count("--package-policy-env CI_PACKAGE_POLICY") == 2
    assert "source_sha: ${{ steps.select.outputs.source_sha }}" in core
    assert "value: ${{ jobs.plan.outputs.source_sha }}" in core
    for filename in ("_ci-crypto.yml", "_ci-web.yml"):
        selected = workflows[filename]
        assert selected.startswith("name: Reusable / ") and "workflow_call:" in selected
        if filename == "_ci-web.yml":
            assert "workflow_dispatch:" not in selected
        else:
            events = selected.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
            assert sorted(mapping_keys(events, 2)) == ["workflow_call", "workflow_dispatch"]
            manual = events.split("  workflow_dispatch:\n", 1)[1]
            assert mapping_keys(manual, 6) == ["source_sha"]
            assert "        type: string\n        required: true" in manual
            assert mapping_keys(selected.split("\njobs:\n", 1)[1], 2) == ["build"]
            crypto_job = block(filename, "build")
            assert 're.fullmatch(r"[0-9a-f]{40}", os.environ["SOURCE_SHA"])' in crypto_job
            assert "SOURCE_SHA: ${{ inputs.source_sha }}" in crypto_job
            assert "Expected a clean checkout of source_sha" in crypto_job
            assert "Source changed during browser cryptography verification" in crypto_job
            assert "crypto-web-provenance-${{ inputs.source_sha }}" in crypto_job
            assert "ref: ${{ github.workflow_sha }}" in crypto_job
            assert "CONTROLLER_TOOLCHAIN: .artifacts/browser-crypto/workflow" in crypto_job
            assert "sparse-checkout: ${{ env.TOOLCHAIN_FILES }}" in crypto_job
            controller_guard = "if: steps.source.outputs.toolchain_origin == 'controller'"
            source_guard = "if: steps.source.outputs.toolchain_origin == 'source'"
            assert controller_guard + "\n        with:\n          ref: ${{ github.workflow_sha }}" in crypto_job
            for action in ("setup-js", "setup-rust", "setup-web-crypto"):
                assert f"/.github/actions/{action}/action.yml" in crypto_job
                assert f"uses: ./.github/actions/{action}\n        {source_guard}" in crypto_job
                assert (f"uses: ./.artifacts/browser-crypto/workflow/.github/actions/{action}\n"
                        f"        {controller_guard}") in crypto_job
            assert "/scripts/wasm-bindgen-version.py" in crypto_job
            assert "Browser cryptography receipt differs from the source toolchain" in crypto_job
            assert 'receipt["toolchain"] = toolchain' in crypto_job
        assert "ref: ${{ inputs.source_sha }}" in selected or "ref: '${{ inputs.source_sha }}'" in selected
    for job, filename in (("crypto-web", "_ci-crypto.yml"), ("web", "_ci-web.yml")):
        assert f"uses: ./.github/workflows/{filename}" in block("ci.yml", job)
        assert "${{ inputs.source_sha }}" in workflows[filename]
    assert "node scripts/crypto-client.mjs --self-test --native-probe" in workflows["_ci-crypto.yml"]
    assert "bash scripts/build-web.sh\n          git diff --exit-code" in workflows["_ci-web.yml"]
    web_build = (root / "scripts/build-web.sh").read_text()
    web_checks = (root / "scripts/check-web.sh").read_text()
    check_commands = [line for line in web_checks.splitlines() if line.startswith("pnpm ")]
    assert check_commands == ["pnpm run generate-routes", "pnpm run test", "pnpm run typecheck",
                              "pnpm --filter @vibeface/client run test", "pnpm --filter @vibeface/client run typecheck"]
    assert web_build.count('bash "$repo/scripts/check-web.sh"') == 1
    assert web_build.count("pnpm install ") == 1
    for command in check_commands[1:]:
        assert command not in web_build and command not in block("ci.yml", "web-tests"), "Web checks belong to check-web.sh"
    assert "pnpm install " not in workflows["release-app.yml"], "build-web owns the release dependency install"
    assert "          scripts/build-web-crypto.sh\n          scripts/build-web.sh\n" in workflows["release-app.yml"]
    for command in ("atlas migrate validate", "python3 scripts/check-source.py", "scripts/check-delivery.py --check-fixtures", "scripts/check-delivery.py", "cargo fmt --all", "bash scripts/check-web.sh"):
        assert command in core, command
    package = block("ci.yml", "package")
    for command in ("--web-only", "--delivery-charity", "--phase1", "--artifacts .artifacts/ci-multi-node", "--chat-agent", "--phase4", "--public-node", "--mesh", "--phase5", "Verify Developer ID signature", "Verify checksum and unpacked source identity"):
        assert command in package, command
    for job in ("npm-package", "npm-smoke"):
        assert "needs.plan.outputs.npm == 'true'" in block("ci.yml", job)
    assert "e2e" not in block("ci.yml", "package")
    assert "cargo test -p vibeface-desktop --features native-ui-tests --locked" in block("ci.yml", "desktop-check")
    assert "cargo clippy -p vibeface-desktop --all-targets --features native-ui-tests --locked -- -D warnings" in block("ci.yml", "desktop-check")
    print("Workflow coverage inventory and Core workflow contracts passed")

def workflow_and_installer():
    root = ROOT
    workflow = (root / ".github/workflows/ci.yml").read_text()
    ci_gate = workflow.split("\n  ci-gate:\n", 1)[1]
    for job, output in (("mobile-check", "mobile"), ("desktop-check", "desktop")):
        assert f"{output}: ${{{{ steps.select.outputs.{output} }}}}" in workflow
        block = re.search(rf"(?ms)^  {job}:\n(.*?)(?=^  [\w-]+:\n|\Z)", workflow)[1]
        assert "    needs: plan\n" in block and f"needs.plan.outputs.{output} == 'true'" in block
        assert job in ci_gate.split("    runs-on:", 1)[0]
    web_build = (root / "scripts/build-web.sh").read_text()
    assert '--filter "vibeface-web..."' in web_build, "Web build installs the Web package closure"
    assert "pnpm --filter @vibeface/secure-native run typecheck" not in web_build
    assert "pnpm --filter vibeface-mobile run typecheck" not in web_build
    concurrency = workflow.split("concurrency:\n", 1)[1].split("\npermissions:", 1)[0]
    assert "ci-core-${{ github.workflow }}-${{ github.event_name }}-${{ github.ref }}" in concurrency
    assert "cancel-in-progress: ${{ inputs.package_policy != 'linux-native' && github.run_attempt == 1 && (github.event_name == 'push' || github.event_name == 'workflow_dispatch') }}" in concurrency
    assert "fetch-depth: 0" in workflow.split("\n  ci-gate:\n", 1)[1]
    for job in ("rust-check", "rust-fast", "rust-slow"):
        block = re.search(rf"(?ms)^  {job}:\n(.*?)(?=^  \w|\Z)", workflow)[1]
        dependency = "[plan, quick-check]" if job == "rust-check" else "plan"
        assert f"    needs: {dependency}\n" in block
        assert "actions/download-artifact@" not in block
    package = re.search(r"(?ms)^  package:\n(.*?)(?=^  \w|\Z)", workflow)[1]
    assert "    needs: [plan, crypto-web, web]\n" in package
    assert "name: crypto-web-${{ github.sha }}" in package and "name: web-${{ github.sha }}" in package
    workflow_contract_self_test()
    quick = re.search(r"(?ms)^  quick-check:\n(.*?)(?=^  \w|\Z)", workflow)[1]
    assert "fetch-depth: 0" in quick, "Release CLI baseline tests require repository history"
    assert quick.count("python3 scripts/check-source.py") == 1
    assert all(name in quick for name in ("scripts/verify-delivery-protected.py", "scripts/verify-delivery-restart-observer.py"))
    config = (root / "crates/control/src/lib.rs").read_text()
    protocol, epoch = (re.search(rf"^pub const {name}: u32 = (\d+);$", config, re.M)[1] for name in ("UPDATE_PROTOCOL", "DATA_EPOCH"))
    installer = (root / "install.sh").read_text()
    assert f"AUTO16_UPDATE_PROTOCOL='{protocol}'" in installer and f"AUTO16_DATA_EPOCH='{epoch}'" in installer, "generated installer contract matches active Rust contracts"
    assert (root / "scripts/generate-installer-contract.mjs").exists() and (root / "scripts/test-installer-contract.mjs").exists()
