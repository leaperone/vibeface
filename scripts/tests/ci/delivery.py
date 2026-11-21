"""Native package policy and controlled delivery coverage cases."""

import argparse
import json
import os
from pathlib import Path
import tempfile

from ci import (
    JOBS,
    event_plan,
    gate,
    make_plan,
    package_policy,
    write_plan,
)
from .support import expect_failure


def native_delivery_self_test(scratch):
    from unittest.mock import patch
    for event in ("pull_request", "push", "schedule", "merge_group"):
        assert package_policy("standard", event) == "standard"
        expect_failure(lambda: make_plan([], event, package_policy="linux-native"))
    for invalid in (0, 1, None, True, False, "", "true", "native", [], {}):
        expect_failure(lambda: package_policy(invalid, "workflow_dispatch"))
        expect_failure(lambda: make_plan([], "workflow_dispatch", package_policy=invalid))
    with tempfile.TemporaryDirectory(prefix="native-event-", dir=scratch) as directory:
        directory = Path(directory)
        event_file = directory / "event.json"
        head = "1" * 40
        environment = {"GITHUB_ACTIONS": "true", "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF_TYPE": "branch",
                       "GITHUB_EVENT_PATH": str(event_file), "GITHUB_SHA": head}
        arguments = argparse.Namespace(event=None, base=None, head=None, package_policy="standard")
        metadata = {"channel": "hourly", "version": "1.2.3-hourly.2026110907", "msrv": "1.91", "prerelease": True}
        def needs_for(selected):
            needs = {job: {"result": "success" if job in selected["required_jobs"] else "skipped"} for job in JOBS}
            needs["plan"] = {"result": "success", "outputs": {"plan": json.dumps(selected)}}
            return needs
        with patch.dict(os.environ, environment), patch("ci.git", return_value=(head + "\n").encode()), \
             patch("ci.release_metadata", return_value=metadata):
            event_file.write_text(json.dumps({"inputs": {}}))
            for policy in ("standard", "linux-native"):
                arguments.package_policy = policy
                selected = event_plan(arguments)
                assert selected["package_policy"] == policy
                gate(needs_for(selected), policy=policy)
                opposite = "standard" if policy == "linux-native" else "linux-native"
                expect_failure(lambda: gate(needs_for(selected), policy=opposite))
                other = dict(make_plan([], "workflow_dispatch", package_policy=opposite), **metadata)
                other.update({key: selected[key] for key in ("coverage_base", "coverage_head", "coverage_reason", "baseline_run_id")})
                expect_failure(lambda: gate(needs_for(other), policy=policy))
                for key in ("package_policy", "package_matrix", "npm", "required_jobs", "check_msrv", "test_tiers", "delivery"):
                    wrong = other[key] if other[key] != selected[key] else ([] if isinstance(selected[key], list) else not selected[key])
                    changed = dict(selected, **{key: wrong})
                    expect_failure(lambda: gate(needs_for(changed), policy=policy))
                for invalid in (0, 1, None, True, "native"):
                    expect_failure(lambda: gate(needs_for(dict(selected, package_policy=invalid)), policy=policy))
                output, summary = directory / f"output-{policy}", directory / f"summary-{policy}"
                with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                    write_plan(selected, output)
                outputs = dict(line.split("=", 1) for line in output.read_text().splitlines())
                assert outputs["source_sha"] == head and json.loads(outputs["plan"]) == selected
                assert f"Package policy: {policy}" in summary.read_text()
            # Caller inputs cannot select or weaken the internal reusable policy.
            for inputs in ({"package_policy": "linux-native"}, {"delivery_live": True}, {"desktop": True}, {"delivery_small": True}, {"windows_only": True}, {"desktop_only": True}, {"mobile_apps": "both"}):
                event_file.write_text(json.dumps({"inputs": inputs}))
                arguments.package_policy = "standard"
                selected = event_plan(arguments)
                assert selected["package_policy"] == "standard" and selected["npm"]
                gate(needs_for(selected))
            arguments.package_policy = "invalid"
            expect_failure(lambda: event_plan(arguments))
    print("Core package policies passed; frozen scope, explicit policy and tampering rejection agree")
