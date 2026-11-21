"""Run live-delivery self-checks with the production entrypoint as the test subject."""

import json
from pathlib import Path
import runpy
import tempfile
from unittest import TestCase

from .test_live_exports import check_protected_dispatch_and_exports
from .test_live_observer import check_observer_summary
from .test_live_source import exact_head_and_clean_tree
from .test_supervision import supervision_controls


def run_live_tests(live):
    from unittest.mock import patch
    with patch("subprocess.check_output", side_effect=["1" * 40, ""]):
        live.source_commit("1" * 40)
    for actual, dirty in (("2" * 40, ""), ("1" * 40, " M tracked.cpp\n"), ("1" * 40, "?? generated.json\n")):
        with patch("subprocess.check_output", side_effect=[actual, dirty]):
            try:
                live.source_commit("1" * 40)
            except ValueError as error:
                details = json.loads(str(error).split(": ", 1)[1])
                assert details == {"expected": "1" * 40, "actual": actual, "changes": dirty.splitlines()}
            else:
                raise AssertionError("Wrong commits and both tracked and untracked drift must fail with bounded filename diagnostics")
    checks = runpy.run_path(str(live.ROOT / "scripts/check-delivery.py"))
    _, original = checks["sandbox_commands"](Path("/context"))
    snapshot = {"memory.events": "low 0\nhigh 0\nmax 1\noom 2\noom_kill 1\noom_group_kill 0\n"}
    assert not any(live.oom_delta(snapshot, {"memory.events": snapshot["memory.events"].replace("max 1", "max 99")}).values())
    for name, before in (("oom", 2), ("oom_kill", 1), ("oom_group_kill", 0)):
        changed = {"memory.events": snapshot["memory.events"].replace(f"{name} {before}\n", f"{name} {before + 1}\n")}
        assert live.oom_delta(snapshot, changed)[name] == 1
    for events in ("oom 1\noom_kill 1\n", "oom 2\n", "oom 2\noom_kill -1\n"):
        with TestCase().assertRaises(ValueError, msg='Decreasing, missing or invalid OOM counters must fail'):
            live.oom_delta(snapshot, {"memory.events": events})
    with tempfile.TemporaryDirectory(prefix="live-self-test-", dir=live.ARTIFACTS) as directory:
        exact_head_and_clean_tree(Path(directory))
        file = Path(directory) / "private.json"
        live.write_private(file, {"configured": True})
        assert file.stat().st_mode & 0o777 == 0o600 and json.loads(live.regular(file)) == {"configured": True}
        log = Path(directory) / "process.log"
        log.write_text("business or fixture failure")
        assert not live.glibc_failure(Path(directory))
        log.write_text("vibeface: symbol lookup error: lib.so: undefined symbol: function, version GLIBC_2.39")
        assert live.glibc_failure(Path(directory))
        with patch.object(live, "command", side_effect=RuntimeError("business failure")), patch.object(live, "glibc_failure", return_value=False):
            with TestCase().assertRaises(RuntimeError, msg='Business failures must not enable a source-build fallback'):
                live.runtime(Path("/binary"), Path("/module"), Path(directory), log, {})
        with patch.object(live, "command", side_effect=RuntimeError("loader failure")), patch.object(live, "glibc_failure", return_value=True):
            assert live.runtime(Path("/binary"), Path("/module"), Path(directory), log, {}) is False
        with patch.object(live, "ROOT", Path(directory)):
            generated = Path(directory) / "apps/app/generated"
            generated.mkdir(parents=True)
            sample = Path(directory) / "package"
            (sample / "web").mkdir(parents=True)
            (sample / "vibeface").write_bytes(b"source-provenance-fixture-only")
            (sample / "vibeface-core").write_bytes(b"source-provenance-core-fixture-only")
            for name in ("_shell.html", "index.html", "_redirects", "_headers", ".vibeface-build.json"):
                (sample / "web" / name).write_text("fixture")
            for name in ("vibeface_secure.js", "vibeface_secure_bg.wasm"):
                (generated / name).write_bytes(b"same-run-crypto")
                (sample / "web" / name).write_bytes(b"same-run-crypto")
            info = {"version": "0.0.1", "target": "x86_64-unknown-linux-gnu", "source_commit": "1" * 40,
                    "update_protocol": 1, "data_epoch": 1, "core_version": "0.0.1", "core_source_commit": "1" * 40,
                    "app_version": "0.0.1", "app_source_commit": "1" * 40}
            (sample / "BUILD-INFO.json").write_text(json.dumps(info))
            live.checked_package(sample, "1" * 40)
            (sample / "BUILD-INFO.json").write_text(json.dumps({**info, "core_version": "2.3.4", "app_version": "3.4.5-rc.2"}))
            live.checked_package(sample, "1" * 40)
            for malformed in ({**info, "source_commit": "2" * 40}, {**info, "target": "another-target"},
                              {**info, "core_source_commit": "2" * 40}, {**info, "app_source_commit": "3" * 40}):
                (sample / "BUILD-INFO.json").write_text(json.dumps(malformed))
                with TestCase().assertRaises(ValueError, msg='Stale or foreign package provenance must fail'):
                    live.checked_package(sample, "1" * 40)
            (sample / "BUILD-INFO.json").write_text(json.dumps(info))
            (sample / "web/vibeface_secure.js").write_bytes(b"different-build")
            with TestCase().assertRaises(ValueError, msg='A different crypto artifact must fail'):
                live.checked_package(sample, "1" * 40)
    check_protected_dispatch_and_exports(live, original)
    run_protected_selfcheck(live)
    print("Protected package, dispatch and export self-tests passed; no Node, container or proof ran")


def run_protected_selfcheck(live):
    driver = runpy.run_path(str(live.ROOT / "scripts/verify-node.py"))
    driver["delivery_protected_selfcheck"]()
    runpy.run_path(str(live.ROOT / live.OBSERVER_SCRIPT))["self_test"]()
    supervision_controls(live)
    check_observer_summary(live)
