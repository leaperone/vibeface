"""Git ranges, cumulative coverage and release metadata cases."""

import argparse
import http.client
import json
import os
from pathlib import Path
import tempfile
import urllib.request

from ci import (
    JOBS,
    PACKAGES,
    changed_paths,
    event_plan,
    gate,
    git,
    make_plan,
    release_metadata,
    successful_push_base,
)
from .support import expect_failure


def coverage_self_test(directory, base, nonancestor):
    import io
    from unittest.mock import patch

    event_file = Path(directory) / "event.json"
    event_file.unlink(missing_ok=True)

    def commit(message, paths=(".",)):
        git("add", "-A", "--", *paths, cwd=directory)
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
    with patch.dict(os.environ, environment), patch("ci.git", side_effect=lambda *args, **kwargs: fixture_git(*args, cwd=directory)), patch("ci.release_metadata", return_value={"channel": "hourly"}):
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
        unit = Path(directory) / "apps/app/src/lib/example.test.ts"
        unit.parent.mkdir(parents=True, exist_ok=True)
        unit.write_text("test-only change\n")
        unit_head = commit("test-only change after unverified Rust", ("apps/app/src/lib/example.test.ts",))
        event_file.write_text(json.dumps({"before": head, "after": unit_head}))
        with patch.dict(os.environ, {"GITHUB_SHA": unit_head}):
            with patch("urllib.request.urlopen", return_value=io.BytesIO(json.dumps({"workflow_runs": [successful]}).encode())):
                accumulated = event_plan(args)
                assert accumulated["rust_packages"] == ["vibeface-core"] and "package" in accumulated["required_jobs"]
            with patch("urllib.request.urlopen", return_value=io.BytesIO(json.dumps({"workflow_runs": [dict(successful, head_sha=head)]}).encode())):
                only_tests = event_plan(args)
                assert only_tests["required_jobs"] == ["quick-check", "crypto-web", "web-tests"]
                unit_needs = {job: {"result": "success" if job in only_tests["required_jobs"] else "skipped"} for job in JOBS}
                unit_needs["plan"] = {"result": "success"}
                gate(unit_needs, only_tests)
                expect_failure(lambda: gate(unit_needs, dict(only_tests, web_tests=False)))
    print("Cumulative push coverage, API fallback and frozen gate self-tests passed")

def changed_ranges_and_metadata(scratch):
    from unittest.mock import patch

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
        with patch.dict(os.environ, environment), patch("ci.git", side_effect=lambda *args, **kwargs: fixture_git(*args, cwd=directory)), patch("ci.changed_paths", return_value=paths) as diff, patch("ci.release_metadata", return_value={"channel": "hourly"}):
            for draft in (True, False):
                event_file.write_text(json.dumps({"pull_request": {"base": {"sha": new_base}, "head": {"sha": pr_head}, "draft": draft}}))
                arguments.base = None
                selected = event_plan(arguments)
                diff.assert_called_with(new_base, pr_head, "pull_request")
                assert selected == dict(make_plan(paths), coverage_base=new_base, coverage_head=pr_head, coverage_reason="event-range", baseline_run_id=None)
                assert selected["test_tiers"] == ["fast"]
        coverage_self_test(directory, base, new_base)
        for version, channel in (("1.2.3", "stable"), ("1.2.3-rc.2", "rc"), ("1.2.3-hourly.2026101923", "hourly")):
            cargo_metadata = {"packages": [{"name": "vibeface", "version": version, "rust_version": "1.89"}]}
            with patch("subprocess.check_output", return_value=json.dumps(cargo_metadata)), patch("ci.git", return_value=b"a" * 40), patch("release_source.validate_source") as ancestor:
                assert release_metadata("branch", "")["channel"] == channel
                assert release_metadata("tag", f"cli-v{version}")["prerelease"] == (channel != "stable")
                ancestor.assert_called_once_with("a" * 40, "cli")
                expect_failure(lambda: release_metadata("tag", "cli-v0.0.0"))
                for other in ("core", "app", "desktop", "mobile", "www"):
                    expect_failure(lambda: release_metadata("tag", f"{other}-v{version}"))
        for version in ("01.2.3", "1.2.3-nightly.20261019.1", "1.2.3-hourly.2026022912", "1.2.3-hourly.2026101924", "1.2.3-hourly.20261019", "1.2.3-hourly.2026101900.1", "1.2.3+build"):
            cargo_metadata = {"packages": [{"name": "vibeface", "version": version, "rust_version": "1.89"}]}
            with patch("subprocess.check_output", return_value=json.dumps(cargo_metadata)):
                expect_failure(lambda: release_metadata("branch", ""))
