"""Offline protected-delivery dispatch and public export checks."""

import argparse
import json
import os
from pathlib import Path
import re
import runpy
import signal
import subprocess
import tempfile
import time
import uuid

from .test_live_observer import observer_fixture


def check_protected_dispatch_and_exports(live, original):
    from copy import deepcopy
    from unittest.mock import Mock, patch
    expected = "1" * 40
    package, run = live.ROOT / ".artifacts/package", live.ROOT / ".artifacts/protected-run"
    common = Path("/fixture/repository.git")
    for git_directory in (common, common / "worktrees/task"):
        command_line = live.protected_container(original, package, expected, run, common, git_directory, "protected-fixture")
        mounts = [command_line[i+1] for i,item in enumerate(command_line) if item == "--mount"]
        assert f"type=bind,source={common},target=/git-common,readonly" in mounts
        assert f"type=bind,source={live.ROOT / '.git'},target=/work/.git,readonly" in mounts
        environment = dict(command_line[i+1].split("=",1) for i,item in enumerate(command_line) if item == "--env")
        assert environment["GIT_DIR"] == str(Path("/git-common") / git_directory.relative_to(common))
        assert environment["GIT_WORK_TREE"] == "/work" and environment["GIT_OPTIONAL_LOCKS"] == "0"
        assert environment["TOKIO_WORKER_THREADS"] == "2"
        assert "VIBEFACE_DELIVERY_DEADLINE_UNIX_MS" not in environment
        assert command_line[command_line.index("--user")+1] == f"{os.getuid()}:{os.getgid()}"
        assert "--network" not in command_line and "--privileged" not in command_line
        assert "--protected-package" in command_line and "--protected-inside" in command_line
        assert "--android" not in command_line and "--package" not in command_line
        assert command_line[command_line.index("--memory")+1] == command_line[command_line.index("--memory-swap")+1] == "11g"
        assert command_line[command_line.index("--pids-limit")+1] == "64"
    with tempfile.TemporaryDirectory(prefix="protected-stage-bound-selftest-", dir=live.ARTIFACTS) as temporary:
        package = Path(temporary) / "package"; package.mkdir()
        (package / "BUILD-INFO.json").write_text("{}")
        run = Path(temporary) / "run"
        process = Mock(wait=Mock(return_value=0), poll=Mock(return_value=0))
        def forbidden(*_args, **_kwargs): raise AssertionError("stage-bound selfcheck opened a network socket")
        with patch.object(live, "source_commit"), patch.object(live, "checked_package"), \
             patch.object(live, "command") as build, patch.object(live, "protected_public", return_value=(run / "public", True)), \
             patch("runpy.run_path", return_value={"sandbox_commands": lambda _context: (["synthetic", "build"], original)}), \
             patch("subprocess.check_output", side_effect=[str(common) + "\n", str(common / "worktrees/task") + "\n"]), \
             patch("subprocess.Popen", return_value=process), \
             patch("subprocess.run", return_value=subprocess.CompletedProcess([], 1, b"", b"No such container")), \
             patch("socket.socket", forbidden), patch("time.time", side_effect=[0, 20_701]), \
             patch.dict(os.environ, {"GITHUB_OUTPUT": ""}), patch("builtins.print"):
            live.protected_package_dispatch(package, expected, run)
            process.wait.assert_called_once_with()
            previous_term = signal.getsignal(signal.SIGTERM)
            terminated = Mock(wait=Mock(side_effect=lambda: os.kill(os.getpid(), signal.SIGTERM) or time.sleep(5)), poll=Mock(side_effect=[None, 0]))
            with patch("subprocess.Popen", return_value=terminated), patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, b"", b"")) as cleanup, \
                 patch("subprocess.check_output", side_effect=[str(common) + "\n", str(common / "worktrees/task") + "\n"]):
                try: live.protected_package_dispatch(package, expected, Path(temporary) / "terminated")
                except InterruptedError: pass
                else: raise AssertionError("a terminated protected host must not report success")
            assert [call.args[0][:2] for call in cleanup.call_args_list] == [["docker", "stop"], ["docker", "rm"]], "a terminated host must stop its container"
            assert signal.getsignal(signal.SIGTERM) is previous_term
        assert "timeout" not in build.call_args.kwargs, "the native stages must not inherit the old UI total timeout"
    with tempfile.TemporaryDirectory(prefix="protected-npm-cache-selftest-", dir=live.ARTIFACTS) as temporary:
        run = Path(temporary); package = run / "package"; package.mkdir()
        snapshot = {"memory.events":"oom 0\noom_kill 0\noom_group_kill 0\n"}
        with patch.object(live, "source_commit"), patch.object(live, "resources", return_value=(Path('/synthetic-cgroup'),snapshot)), \
             patch.object(live, "command", side_effect=RuntimeError("synthetic stop before artifact preparation")) as prepare, \
             patch("subprocess.Popen", forbidden), patch("subprocess.run", forbidden), patch("socket.socket", forbidden), \
             patch.dict(os.environ,{"HOME":"/home/runner","TOKIO_WORKER_THREADS":"2","RISC0_HOME":"/risc0", \
                 "RISC0_SERVER_PATH":"/opt/vdp/bin/r0vm","npm_config_cache":"/home/runner/.npm"},clear=True):
            try: live.protected_package_inside(package,expected,run)
            except RuntimeError as error: assert str(error) == "Protected sandbox failed; only independently verified partial evidence may be exported"
            else: raise AssertionError("the preparation selfcheck must stop before real work")
            prepare.assert_called_once()
            environment = prepare.call_args.kwargs["env"]
            assert environment["npm_config_cache"] == os.environ["npm_config_cache"] == str(run / "npm-cache")
            assert environment["HOME"] == "/home/runner" and environment["RISC0_HOME"] == "/risc0"
            assert environment["RISC0_SERVER_PATH"] == "/opt/vdp/bin/r0vm" and environment["TOKIO_WORKER_THREADS"] == "2"
            assert not any(name.startswith(("RISC0_DEV","RISC0_SKIP")) for name in environment)
    driver = runpy.run_path(str(live.ROOT / "scripts/verify-node.py"))
    with tempfile.TemporaryDirectory(prefix="protected-public-node-selftest-", dir=live.ARTIFACTS) as temporary:
        directory = Path(temporary)
        binary = directory / "vibeface"; binary.touch(mode=0o600)
        options = argparse.Namespace(binary=binary, artifacts=directory / "fixtures",
            crypto_module=directory / "vibeface_secure.js", fixture_kind="mesh", fixture_port=27100)
        class FixtureConstructed(BaseException): pass
        mesh = "t-123456789abc"
        public = Mock(device_id="11" * 32, quic_port=27102, directory=directory / "public",
            mesh_address="vibeface://node/" + "11" * 32 + "?addr=127.0.0.1:27102")
        member = Mock(directory=directory / "member")
        for node in (public, member):
            node.directory.mkdir()
            (node.directory / "config.json").write_text('{"bins":{}}')
        public.start.side_effect = FixtureConstructed
        constructor = Mock(side_effect=[public, member])
        cleanup = Mock()
        configure = Mock()
        relay_args = ["--relay-listen", "127.0.0.1:27101", "--relay-url", "http://127.0.0.1:27101",
            "--relay-open", "--relay-rate", "1048576", "--relay-max-clients", "64", "--relay-max-clients-per-ip", "64"]
        with patch.dict(driver["e2e_fixture"].__globals__, Node=constructor, available_port=lambda: 27101,
                        mesh_network=lambda: mesh, node_cli=configure, stop_fixture_processes=cleanup), patch("signal.signal"), \
             patch("subprocess.Popen", forbidden), patch("subprocess.run", forbidden), patch("socket.socket", forbidden):
            assert driver["public_node_args"]() == relay_args
            try: driver["e2e_fixture"](options)
            except FixtureConstructed: pass
            else: raise AssertionError("public Node fixture selfcheck must stop before process startup")
        assert len(constructor.call_args_list) == 2
        public_call, member_call = constructor.call_args_list
        run = public_call.args[0]
        assert public_call.args[1] == "public" and member_call.args[1] == "member"
        assert public_call.kwargs == {"port": options.fixture_port,
            "init_args": ["--mesh-network", mesh, "--forward-any", *relay_args]}
        assert member_call.kwargs == {"init_args": ["--root", str(run / "workspace"), "--mesh-network", mesh,
            "--bootstrap", public.mesh_address]}
        public.configure_mesh.assert_called_once_with(mesh)
        configure.assert_called_once_with(member, "config", "set", "network.mesh.allow_private", "true")
        public.start.assert_called_once_with()
        member.start.assert_not_called()
        for node in (public, member):
            node.client.close.assert_called_once_with()
            node.stop.assert_called_once_with()
        cleanup.assert_called_once_with(run)
    with tempfile.TemporaryDirectory(prefix="protected-export-selftest-", dir=live.ARTIFACTS) as temporary:
        run = Path(temporary); public = run / "native/public"; public.mkdir(parents=True)
        facts = []
        for relation, order in [(1,None),(2,"ab"*32),(3,"ab"*32),(2,"cd"*32),(3,"cd"*32)]:
            name = "key-binding" if relation == 1 else f"order-{order}-relation-{relation}-window-0"
            receipt = public / (name+".receipt"); receipt.write_bytes(b"synthetic exporter selfcheck only: "+name.encode())
            metadata = public / (name+".json"); metadata.write_text(json.dumps({"journalHash":"a"*64}))
            fact = {"relation":relation,"public":name+".json","receipt":name+".receipt","receiptSha256":live.digest(receipt),"publicSha256":live.digest(metadata),"journalHash":"a"*64,
                "official_host_verified":True,"wrong_journal_receipt_comparison_rejected":True}
            if order: fact.update(orderId="0x"+order,windowIndex=0)
            facts.append(fact)
        observer_fixture(live, run,expected)
        observer_record,observer_passed = live.write_observer_summary(run,expected,0,0)
        assert observer_passed
        terminal = public / "native-result.json"
        base = {"source_commit":expected,"scope":"fresh-two-order-protected-native-only","status":"PASS","genuine_receipts":facts,
            "public_files":{"native-result.json":live.digest(terminal)}}
        (run / "artifact-provenance.json").write_text(json.dumps({"sourceCommit":expected,"status":"PASS","restartObserver":observer_record}))
        (run / "resources.json").write_text(json.dumps({"status":"PASS"}))
        def export(manifest, returncode=0):
            (public / "progress.json").write_text(json.dumps(manifest))
            output, passed = live.protected_public(run, expected, returncode)
            summary = json.loads((output / "summary.json").read_text())
            assert summary["status"] == ("PASS" if passed else "FAIL")
            assert summary["nativeStatus"] in ("PASS","FAIL","RUNNING","INTERRUPTED","UNVERIFIED")
            assert all(re.fullmatch(r"[a-z_]+",code) for code in summary["failureCodes"])
            assert len(list(output.glob("*.receipt"))) == summary["genuineReceipts"]
            return output, passed, summary
        output, passed, summary = export(base)
        assert passed and summary["genuineReceipts"] == 5
        assert (output / "key-binding.receipt").stat().st_mode & 0o777 == 0o644
        assert json.loads((output / "artifact-provenance.json").read_bytes())["restartObserver"]["sha256"] == live.digest(output / "restart-observer-summary.json")
        observer_path = run / "restart-observer-summary.json"
        provenance_path = run / "artifact-provenance.json"
        observer_bytes,provenance_bytes = observer_path.read_bytes(),provenance_path.read_bytes()
        observer_value = json.loads(observer_bytes)
        for change in ({"raw_diagnostic":"UNAPPROVED_PRIVATE_SENTINEL"},{"status":"UNKNOWN"},{"observerExitCode":1},
                       {"nativeExitCode":1},{"windowStarted":False},{"calibrationOpens":0},{"rawTransactionsAfter":8},
                       {"chainSampleErrors":None},{"prewindowSnapshotMissing":None},
                       *({name:"0"*64} for name in live.OBSERVER_BINDINGS)):
            observer_path.write_text(json.dumps({**observer_value,**change}))
            provenance = json.loads(provenance_bytes)
            provenance["restartObserver"]["sha256"] = live.digest(observer_path)
            provenance_path.write_text(json.dumps(provenance))
            rejected,passed,details = export(base)
            assert not passed and details["genuineReceipts"]==5 and details["restartObserverStatus"]=="UNVERIFIED"
            assert not (rejected / "restart-observer-summary.json").exists()
            assert all(b"UNAPPROVED_PRIVATE_SENTINEL" not in path.read_bytes() for path in rejected.iterdir())
        observer_path.write_bytes(observer_bytes); provenance_path.write_bytes(provenance_bytes)
        for mode in ("missing","changed","unapproved_manifest","exit_mismatch"):
            provenance = json.loads(provenance_bytes)
            if mode=="missing": observer_path.unlink()
            elif mode=="changed": observer_path.write_bytes(observer_bytes+b" ")
            elif mode=="unapproved_manifest": provenance["restartObserver"]["private"]="UNAPPROVED_PRIVATE_SENTINEL"
            else: provenance["restartObserver"]["observerExitCode"]=1
            provenance_path.write_text(json.dumps(provenance))
            rejected,passed,details = export(base)
            assert not passed and details["genuineReceipts"]==5 and details["restartObserverStatus"]=="UNVERIFIED"
            assert all(b"UNAPPROVED_PRIVATE_SENTINEL" not in path.read_bytes() for path in rejected.iterdir())
            observer_path.write_bytes(observer_bytes); provenance_path.write_bytes(provenance_bytes)
        _,passed,_ = export(base)
        assert passed, "observer rejection tests need a restored positive control"
        output, passed, summary = export(base,1)
        assert not passed and summary["genuineReceipts"] == 5
        (public / facts[-1]["public"]).unlink()
        partial = {**deepcopy(base),"status":"FAIL","genuine_receipts":deepcopy(facts[:4])}
        output, passed, summary = export(partial,1)
        assert not passed and summary["genuineReceipts"] == 4 and not (output / facts[-1]["receipt"]).exists()
        sentinel = "UNAPPROVED_PRIVATE_SENTINEL"
        for name in (".progress.json.unregistered-temp", "private-runtime.json"):
            (public / name).write_text(sentinel)
        partial["raw_exception"] = sentinel
        output, passed, summary = export(partial,1)
        assert not passed and summary["genuineReceipts"] == 4
        assert all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())
        assert not (output / "private-runtime.json").exists() and not any(path.name.startswith('.') for path in output.iterdir())
        invalid = deepcopy(partial); invalid["genuine_receipts"][1]["official_host_verified"] = False
        output, passed, summary = export(invalid)
        assert not passed and summary["genuineReceipts"] == 3 and not (output / facts[1]["receipt"]).exists()
        invalid = deepcopy(partial); rejected = deepcopy(facts[0]); rejected["official_host_verified"] = False
        invalid["genuine_receipts"].insert(0,rejected)
        output, passed, summary = export(invalid)
        assert not passed and summary["genuineReceipts"] == 4 and (output / facts[0]["receipt"]).exists()
        receipt = public / facts[0]["receipt"]; original_receipt = receipt.read_bytes(); receipt.write_bytes(b"changed synthetic receipt")
        output, passed, summary = export(partial)
        assert not passed and summary["genuineReceipts"] == 3 and not (output / facts[0]["receipt"]).exists()
        receipt.write_bytes(original_receipt)
        metadata = public / facts[0]["public"]; original_metadata = metadata.read_bytes(); metadata.write_text(json.dumps({"journalHash":"a"*64,"private":sentinel}))
        output, passed, summary = export(partial)
        assert not passed and summary["genuineReceipts"] == 3 and all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())
        metadata.write_bytes(original_metadata)
        invalid = deepcopy(partial); invalid["genuine_receipts"][0]["public"] = sentinel
        output, passed, summary = export(invalid)
        assert not passed and summary["genuineReceipts"] == 3 and all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())
        invalid = deepcopy(partial); invalid["status"] = sentinel
        output, passed, summary = export(invalid)
        assert not passed and summary["nativeStatus"] == "UNVERIFIED" and summary["genuineReceipts"] == 4
        assert all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())
        terminal.write_text(sentinel)
        output, passed, summary = export(partial)
        assert not passed and summary["genuineReceipts"] == 4 and not (output / "native-result.json").exists()
        (public / "progress.json").write_text(sentinel)
        output, passed = live.protected_public(run,expected,1)
        assert not passed and json.loads((output / "summary.json").read_text())["genuineReceipts"] == 0
        assert not (output / "progress.json").exists()
        terminal.write_text(json.dumps({"marker":"synthetic exporter selfcheck only"}))
        (public / "progress.json").write_text(json.dumps(partial))
        package = run / "package"; package.mkdir(); (package / "BUILD-INFO.json").write_text("{}")
        exporter = live.protected_public
        for mode in ("partial","exception"):
            github_output = run / ("github-output-"+mode)
            process = Mock(wait=Mock(return_value=1),poll=Mock(return_value=1))
            def publish(*_args):
                if mode == "exception": raise RuntimeError(sentinel)
                return exporter(run,expected,1)
            with patch.object(live, "source_commit"), patch.object(live, "checked_package"), patch.object(live, "command"), \
                 patch.object(live, "protected_public",side_effect=publish), \
                 patch("runpy.run_path",return_value={"sandbox_commands":lambda _context:(["synthetic","build"],original)}), \
                 patch("subprocess.check_output",side_effect=[str(common)+"\n",str(common/"worktrees/task")+"\n"]), \
                 patch("subprocess.Popen",return_value=process), \
                 patch("subprocess.run",return_value=subprocess.CompletedProcess([],1,b"",b"No such container")), \
                 patch.dict(os.environ,{"GITHUB_OUTPUT":str(github_output)}), patch("builtins.print"):
                try: live.protected_package_dispatch(package,expected,run/("host-"+mode))
                except RuntimeError as error: assert sentinel not in str(error)
                else: raise AssertionError("failed native/export runs cannot pass")
            output = Path(github_output.read_text().strip().split("=",1)[1])
            summary = json.loads((output/"summary.json").read_text())
            assert summary["status"] == "FAIL" and summary["genuineReceipts"] == (4 if mode == "partial" else 0)
            assert all(sentinel.encode() not in path.read_bytes() for path in output.iterdir())


    check_registration_export(live, expected)
    check_proof_export(live, expected)


def check_registration_export(live, expected):
    from copy import deepcopy
    from unittest.mock import patch
    driver = runpy.run_path(str(live.ROOT / "scripts/verify-delivery-protected.py"))
    artifacts = {"prover":"b"*64,"r0vm":"c"*64,"profile":"d"*64}
    records = driver["self_test_diagnostic_fixture"]()
    data = b"".join((json.dumps(record,separators=(",",":"))+"\n").encode() for record in records)
    diagnostic = driver["registration_diagnostics"](data,expected,artifacts,{"exit_code":0,"signal":None,"spawn_errno":None})
    assert diagnostic["availability"]=="complete" and len(diagnostic["resource_deltas"])==8
    def forbidden(*_args,**_kwargs): raise AssertionError("diagnostic export self-test started business work")
    with tempfile.TemporaryDirectory(prefix="registration-diagnostic-export-selftest-",dir=live.ARTIFACTS) as temporary, \
         patch("subprocess.Popen",forbidden),patch("subprocess.run",forbidden),patch("socket.socket",forbidden):
        run=Path(temporary)
        observer_fixture(live, run,expected,{"source_commit":expected,"artifact_sha256":artifacts})
        public=run/"native/public"
        observer,passed=live.write_observer_summary(run,expected,0,0)
        assert passed
        live.write_private(run/"artifact-provenance.json",{"sourceCommit":expected,"status":"PASS","testedArtifactSha256":artifacts,"restartObserver":observer})
        live.write_private(run/"resources.json",{"status":"PASS"})
        facts=[]
        for relation,order in [(1,None),(2,"ab"*32),(3,"ab"*32),(2,"cd"*32),(3,"cd"*32)]:
            name="key-binding" if relation==1 else f"order-{order}-relation-{relation}-window-0"
            receipt=public/(name+".receipt");receipt.write_bytes(b"synthetic diagnostic-export control only: "+name.encode())
            metadata=public/(name+".json");metadata.write_text(json.dumps({"journalHash":"a"*64}))
            fact={"relation":relation,"public":metadata.name,"receipt":receipt.name,"receiptSha256":live.digest(receipt),"publicSha256":live.digest(metadata),
                  "journalHash":"a"*64,"official_host_verified":True,"wrong_journal_receipt_comparison_rejected":True}
            if order: fact.update(orderId="0x"+order,windowIndex=0)
            facts.append(fact)
        def export(value,code=0):
            diagnostic_path=public/"registration-diagnostics.json"
            diagnostic_path.write_text(json.dumps(value))
            manifest={"source_commit":expected,"scope":"fresh-two-order-protected-native-only","status":"PASS","genuine_receipts":facts,
                "public_files":{"native-result.json":live.digest(public/"native-result.json"),"registration-diagnostics.json":live.digest(diagnostic_path)}}
            (public/"progress.json").write_text(json.dumps(manifest))
            return live.protected_public(run,expected,code)
        output,passed=export(diagnostic)
        assert passed
        projected=json.loads((output/"registration-diagnostics.json").read_bytes())
        assert projected==diagnostic and len(projected["events"])==len(records) and len(projected["resource_deltas"])==8
        boundary=next(index for index,event in enumerate(records) if event["phase"]=="worker_wait")
        failed=deepcopy(records[:boundary+1])
        failed[-1].update(edge="error",exit_code=137,failure_class="worker_exit")
        partial=driver["registration_diagnostics"](b"".join((json.dumps(record,separators=(",",":"))+"\n").encode() for record in failed),
            expected,artifacts,{"exit_code":1,"signal":None,"spawn_errno":None})
        assert partial["availability"]=="partial" and partial["first_observed_failure"]["exit_code"]==137
        output,passed=export(partial,1)
        assert not passed and json.loads((output/"registration-diagnostics.json").read_bytes())==partial
        invalid_exit=deepcopy(partial)
        invalid_exit["events"][-1]["exit_code"]=0
        invalid_exit["first_observed_failure"]["exit_code"]=0
        missing_wait_status=deepcopy(diagnostic)
        for event in missing_wait_status["events"]:
            if event["phase"]=="worker_wait": event["exit_code"]=None
        for invalid in (invalid_exit,missing_wait_status):
            output,passed=export(invalid)
            assert not passed and not (output/"registration-diagnostics.json").exists()
        sentinel="UNAPPROVED_PRIVATE_SENTINEL"
        changes=[lambda value:value.update(private=sentinel),lambda value:value.update(source_commit="0"*40),
            lambda value:value["artifact_sha256"].update(prover="0"*64),lambda value:value["events"][0].update(private=sentinel),
            lambda value:value["events"][0].update(exit_code=True),lambda value:value["resource_deltas"].append(deepcopy(value["resource_deltas"][0]))]
        for change in changes:
            invalid=deepcopy(diagnostic);change(invalid)
            output,passed=export(invalid)
            assert not passed and not (output/"registration-diagnostics.json").exists()
            assert all(sentinel.encode() not in item.read_bytes() for item in output.iterdir())
        output,passed=export(diagnostic,1)
        assert not passed and (output/"registration-diagnostics.json").exists()
        unavailable=driver["registration_diagnostics"](b"",expected,artifacts,{"exit_code":None,"signal":9,"spawn_errno":None})
        output,passed=export(unavailable,1)
        assert not passed and json.loads((output/"registration-diagnostics.json").read_bytes())["availability"]=="unavailable"
    print(f"Registration diagnostic export self-tests passed; full synthetic {len(records)}-event path, 8 resource groups, strict privacy negatives; no processes or proofs")


def check_proof_export(live, expected):
    from copy import deepcopy
    from unittest.mock import patch
    driver = runpy.run_path(str(live.ROOT / "scripts/verify-delivery-protected.py"))
    profile_bytes = live.regular(live.ROOT / "crates/delivery/proof/profile.json", 16384)
    profile = driver["profile_id"](driver["diagnostic_json"](profile_bytes))
    profile_value = json.loads(profile_bytes)
    artifacts = {name: "a" * 64 for name in ("binary", "core_binary", "crypto_module", "crypto_wasm", "prover", "r0vm", "profile")}
    artifacts.update(prover="b" * 64, r0vm="c" * 64, profile=live.digest_bytes(profile_bytes))
    guest = {"imageId": bytes(profile_value["guest_image_id"]).hex(), "schemaHash": bytes(profile_value["statement_schema_hash"]).hex(),
             "elfSha256": "e" * 64, "statementBytes": 1504}
    principals = {node: {"peer": value * 32} for node, value in (("S", "11"), ("R", "22"))}
    orders = [{"order_id": value * 32, "sha256": "f" * 64} for value in ("ab", "cd")]
    fixtures = {(node, index): driver["self_test_proof_fixture"](node, index, artifacts, profile) for node in ("S", "R") for index in range(2)}
    facts = [{"relation": 1, "public": "key-binding.json", "receipt": "key-binding.receipt", "journalHash": "a" * 64,
              "official_host_verified": True, "wrong_journal_receipt_comparison_rejected": True}]
    metadata = {"key-binding.json": {"journalHash": "a" * 64, "profileId": profile, "imageId": guest["imageId"]}}
    receipts = {"key-binding.receipt": b"synthetic key receipt for proof diagnostic exporter selfcheck"}
    for (node, index), records in fixtures.items():
        relation, order = records[0]["relation"], orders[index]["order_id"]
        name = f"order-{order}-relation-{relation}-window-0"
        metadata[name + ".json"] = {"journalHash": records[0]["job"], "profileId": profile, "imageId": guest["imageId"]}
        receipts[name + ".receipt"] = b"synthetic order receipt for exporter selfcheck: " + name.encode()
        facts.append({"relation": relation, "orderId": "0x" + order, "windowIndex": 0, "public": name + ".json", "receipt": name + ".receipt",
                      "journalHash": records[0]["job"], "official_host_verified": True, "wrong_journal_receipt_comparison_rejected": True})
    for fact in facts:
        fact["receiptSha256"] = live.digest_bytes(receipts[fact["receipt"]])
        fact["publicSha256"] = live.digest_bytes(json.dumps(metadata[fact["public"]]).encode())
    jobs = [driver["summarize_proof_job"](b"".join((json.dumps(record, separators=(",", ":")) + "\n").encode() for record in records),
        node, index, principals[node]["peer"], orders[index]["order_id"], profile, artifacts, [1, 2], records[0]["job"], facts)
        for (node, index), records in fixtures.items()]
    diagnostic = {"schema_version": 1, "source_commit": expected, "artifact_sha256": {name: artifacts[name] for name in ("prover", "r0vm", "profile")},
                  "profile_id": profile, "counter_fields": list(driver["DIAGNOSTIC_COUNTERS"]), "attempt_policy": "first_admitted", "availability": "complete", "jobs": jobs}
    native = {"source_commit": expected, "artifact_sha256": artifacts, "profile_id": profile, "prover_info": guest,
              "principals": principals, "orders": orders, "genuine_receipts": facts, "proof_diagnostics": diagnostic}
    def forbidden(*_args, **_kwargs): raise AssertionError("proof diagnostic export selfcheck started business work")
    negative_count = 0
    with tempfile.TemporaryDirectory(prefix="proof-diagnostic-export-selftest-", dir=live.ARTIFACTS) as temporary, \
         patch("subprocess.Popen", forbidden), patch("subprocess.run", forbidden), patch("socket.socket", forbidden):
        parent = Path(temporary)
        def export(value, *, code=0, wrapper_artifacts=None, duplicate=False):
            run = parent / uuid.uuid4().hex; run.mkdir(mode=0o700)
            observer_fixture(live, run, expected, value)
            public = run / "native/public"
            for name, data in receipts.items(): (public / name).write_bytes(data)
            for name, data in metadata.items(): (public / name).write_text(json.dumps(data))
            if duplicate:
                path = public / "native-result.json"
                path.write_bytes(path.read_bytes().replace(b'"proof_diagnostics":', b'"proof_diagnostics":null,"proof_diagnostics":', 1))
                # Authenticate the altered original bytes too; the parser, not a stale hash, must reject them.
                bindings = live.observer_bindings(run, expected)
                raw = json.loads((run / "restart-observer/result.json").read_bytes())
                raw["native_result_sha256"] = bindings["nativeResultSha256"]
                (run / "restart-observer/result.json").write_text(json.dumps(raw))
            observer, passed = live.write_observer_summary(run, expected, 0, 0)
            assert passed
            live.write_private(run / "artifact-provenance.json", {"sourceCommit": expected, "status": "PASS", "testedArtifactSha256": wrapper_artifacts or artifacts,
                "guestInfo": guest, "restartObserver": observer})
            live.write_private(run / "resources.json", {"status": "PASS"})
            raw_bytes = (public / "native-result.json").read_bytes()
            live.write_private(public / "progress.json", {"source_commit": expected, "scope": "fresh-two-order-protected-native-only", "status": "PASS",
                "genuine_receipts": facts, "public_files": {"native-result.json": live.digest_bytes(raw_bytes)}})
            output, passed = live.protected_public(run, expected, code)
            return output, passed, raw_bytes
        output, passed, raw_bytes = export(native)
        assert passed and (output / "native-result.json").read_bytes() == raw_bytes
        assert json.loads(raw_bytes)["proof_diagnostics"]["availability"] == "complete" and len(jobs) == 4
        missing = deepcopy(native); missing.pop("proof_diagnostics")
        output, passed, raw_bytes = export(missing)
        assert passed and (output / "native-result.json").read_bytes() == raw_bytes
        unavailable = deepcopy(native)
        unavailable["proof_diagnostics"].update(availability="unavailable", jobs=[driver["unavailable_proof_job"](job["node"], job["order_index"]) for job in jobs])
        for code in (0, 1):
            output, passed, raw_bytes = export(unavailable, code=code)
            assert passed == (code == 0) and (output / "native-result.json").read_bytes() == raw_bytes
        changes = [lambda value: value["proof_diagnostics"].update(source_commit="0" * 40),
                   lambda value: value["proof_diagnostics"]["artifact_sha256"].update(prover="0" * 64),
                   lambda value: value["proof_diagnostics"].update(profile_id="0" * 64),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(node="R"),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(order_index=1),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(relation=3),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(window=1),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(job_digest="0" * 64),
                   lambda value: value["proof_diagnostics"].update(private="PRIVATE_ORDER_DIAGNOSTIC_SENTINEL"),
                   lambda value: value["proof_diagnostics"]["jobs"][0]["source_activity"]["start"].update(private="PRIVATE_ORDER_DIAGNOSTIC_SENTINEL"),
                   lambda value: value["proof_diagnostics"]["jobs"][0].update(mode="receipt_reconcile"),
                   lambda value: value["artifact_sha256"].update(core_binary="0" * 64),
                   lambda value: value["prover_info"].update(elfSha256="0" * 64)]
        for change in changes:
            invalid = deepcopy(native); change(invalid)
            output, passed, _ = export(invalid)
            assert not passed and not (output / "native-result.json").exists()
            assert json.loads((output / "summary.json").read_bytes())["genuineReceipts"] == 5
            assert all(b"PRIVATE_ORDER_DIAGNOSTIC_SENTINEL" not in path.read_bytes() for path in output.iterdir())
            negative_count += 1
        output, passed, _ = export(native, wrapper_artifacts={**artifacts, "r0vm": "0" * 64})
        assert not passed and not (output / "native-result.json").exists(); negative_count += 1
        output, passed, _ = export(native, duplicate=True)
        assert not passed and not (output / "native-result.json").exists(); negative_count += 1
    print(json.dumps({"proof_diagnostic_export_selftest": "PASS", "concrete_jobs": 4, "qualified_pairs": 5,
                      "negative_controls": negative_count, "native_bytes_preserved": True, "external_processes": 0, "network_sockets": 0}))
