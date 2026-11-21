"""Offline fixtures and evidence checks for the protected restart observer."""

import json
from pathlib import Path
import tempfile


def observer_fixture(live, run, expected, native_extra=None):
    configuration = run / "configuration.json"
    live.write_private(configuration,{"source_commit":expected,"sha256":{"prover":"b"*64}})
    terminal = run / "native/public/native-result.json"
    terminal.parent.mkdir(parents=True,exist_ok=True)
    terminal.write_text(json.dumps({"status":"PASS","real_process_restart":{"receiver_history_read":True}, **(native_extra or {})}))
    bindings = live.observer_bindings(run,expected)
    statistics = {"rpc_counts":{"eth_sendRawTransaction":7},"other_logged_methods":0}
    accounts = {name:{"nonce":"0x1","pending_nonce":"0x1","private":"UNAPPROVED_PRIVATE_SENTINEL"} for name in ("S","R","P")}
    raw = {"source_commit":expected,"run_binding_sha256":bindings["runBindingSha256"],
        "configuration_sha256":bindings["configurationSha256"],"script_sha256":bindings["scriptSha256"],
        "prover_sha256":bindings["proverSha256"],"native_result_sha256":bindings["nativeResultSha256"],
        "status":"PASS","window_started":True,"window_ended":True,"prover_opens_in_window":0,"proofs_generated_by_observer":0,
        "calibration":{"status":"PASS","exit_code":0,"observed_inode_open_events":1},"flags":[],
        "window_start":{"rpc_baseline":{**statistics,"state":"ready"}},"window_end":{"rpc_final":{**statistics,"state":"stopped"}},
        "write_rpc_delta":{"eth_sendRawTransaction":0},"prover_inode_and_identity_unchanged":True,
        "first_anchored_snapshot_in_window":{"accounts":accounts},"last_anchored_snapshot_in_window":{"accounts":accounts},
        "chain_samples":2,"chain_sample_errors":0,"prewindow_rpc_snapshot_missing":{"count":0},
        "native_status":"PASS","native_real_process_restart":{"receiver_history_read":True}}
    (run / "restart-observer").mkdir()
    live.write_private(run / "restart-observer/result.json",raw)
    return configuration


def check_observer_summary(live):
    from copy import deepcopy
    expected = "1"*40
    with tempfile.TemporaryDirectory(prefix="protected-observer-summary-selftest-",dir=live.ARTIFACTS) as directory:
        run=Path(directory); observer_fixture(live, run,expected)
        manifest,passed=live.write_observer_summary(run,expected,0,0)
        assert passed
        target=run / manifest["summary"]; positive=json.loads(target.read_bytes()); bindings=live.observer_bindings(run,expected)
        assert b"UNAPPROVED_PRIVATE_SENTINEL" not in target.read_bytes()
        changes = [{"unapproved":"UNAPPROVED_PRIVATE_SENTINEL"},{"status":"UNKNOWN"},{"failureCode":"UNAPPROVED_PRIVATE_SENTINEL"},
                   {"schemaVersion":True},{"rawTransactionsBefore":True},{"nativeExitCode":1},{"observerExitCode":1},
                   {"chainSamples":1},{"calibrationOpens":0},{"proverOpensInWindow":1},{"counterReadyToStopped":False}]
        changes += [{name:"0"*64} for name in live.OBSERVER_BINDINGS]
        changes += [{name:None} for name in live.OBSERVER_COUNTS]
        changes += [{"rawTransactionsAfter":8}]
        for change in changes:
            invalid={**deepcopy(positive),**change}
            try: live.project_observer_summary(invalid,bindings)
            except (ValueError,TypeError): pass
            else: raise AssertionError("an invalid observer summary was certified")
        for native_exit,observer_exit in ((1,0),(0,1),(None,None)):
            target.unlink()
            manifest,passed=live.write_observer_summary(run,expected,native_exit,observer_exit)
            assert not passed and json.loads(target.read_bytes())["status"]=="INCONCLUSIVE"
        raw_path=run / "restart-observer/result.json"; raw=json.loads(raw_path.read_bytes())
        for change in ({"script_sha256":"0"*64},{"window_started":False},{"calibration":{"exit_code":"UNAPPROVED_PRIVATE_SENTINEL"}},
                       {"write_rpc_delta":{"eth_sendRawTransaction":1}}, {"status":"INCONCLUSIVE"}):
            raw_path.write_text(json.dumps({**raw,**change})); target.unlink()
            _,passed=live.write_observer_summary(run,expected,0,0)
            assert not passed and b"UNAPPROVED_PRIVATE_SENTINEL" not in target.read_bytes()
        contradictory=[]
        for method in ("eth_sendRawTransaction","eth_sign","personal_sign","evm_mine","eth_sendTransaction",
                       "eth_signTypedData","personal_unlockAccount","evm_setTime","miner_start","debug_traceTransaction"):
            changed=deepcopy(raw)
            counts=changed["window_end"]["rpc_final"]["rpc_counts"]
            counts[method]=counts.get(method,0)+1
            contradictory.append(("unreported-"+method,changed))
            changed=deepcopy(changed); changed["write_rpc_delta"][method]=1
            contradictory.append(("reported-"+method,changed))
        for declaration in ({},{"eth_sendRawTransaction":False},{"eth_sendRawTransaction":0.0},
                            {"eth_sendRawTransaction":0,"personal_sign":0}):
            contradictory.append(("inexact-or-mismatched-declaration",{**deepcopy(raw),"write_rpc_delta":declaration}))
        changed=deepcopy(raw)
        for snapshot in (changed["window_start"]["rpc_baseline"],changed["window_end"]["rpc_final"]):
            snapshot["rpc_counts"]["personal_sign"]=0
        contradictory.append(("omitted-zero-write-method",changed))
        for boundary in ("window_start","window_end"):
            field="rpc_baseline" if boundary=="window_start" else "rpc_final"
            for invalid in (None,[],{}, {"eth_call":1}, {"eth_sendRawTransaction":7,"eth_call":True},
                            {"eth_sendRawTransaction":7,"eth_call":-1}, {"eth_sendRawTransaction":7,"eth_call":1.0},
                            {"eth_sendRawTransaction":7,"eth_call":"1"}, {"eth_sendRawTransaction":7,"eth_call":None}):
                changed=deepcopy(raw); changed[boundary][field]["rpc_counts"]=invalid
                contradictory.append(("invalid-counter-map-"+boundary,changed))
            changed=deepcopy(raw); del changed[boundary][field]["rpc_counts"]
            contradictory.append(("missing-counter-map-"+boundary,changed))
            changed=deepcopy(raw); changed[boundary][field]["other_logged_methods"]=False
            contradictory.append(("inexact-other-method-count-"+boundary,changed))
        for field in ("chain_samples","chain_sample_errors"):
            for invalid in (None,True,-1,"0"):
                changed=deepcopy(raw); changed[field]=invalid
                contradictory.append(("invalid-"+field,changed))
            changed=deepcopy(raw); del changed[field]
            contradictory.append(("missing-"+field,changed))
        for invalid in (None,False,-1,"0"):
            changed=deepcopy(raw); changed["prewindow_rpc_snapshot_missing"]["count"]=invalid
            contradictory.append(("invalid-prewindow-missing-count",changed))
        changed=deepcopy(raw); del changed["prewindow_rpc_snapshot_missing"]["count"]
        contradictory.append(("missing-prewindow-missing-count",changed))
        for name,changed in contradictory:
            raw_path.write_text(json.dumps(changed)); target.unlink()
            _,passed=live.write_observer_summary(run,expected,0,0)
            assert not passed and json.loads(target.read_bytes())["status"]=="INCONCLUSIVE",name
        restored=deepcopy(raw)
        restored["chain_sample_errors"]=3; restored["prewindow_rpc_snapshot_missing"]["count"]=1
        restored["window_end"]["rpc_final"]["rpc_counts"]["eth_coinbase"]=2
        raw_path.write_text(json.dumps(restored)); target.unlink()
        _,passed=live.write_observer_summary(run,expected,0,0)
        result=json.loads(target.read_bytes())
        assert passed and result["chainSampleErrors"]==3 and result["prewindowSnapshotMissing"]==1
        sentinel = "UNAPPROVED_PRIVATE_SENTINEL"
        projected = live.project_wrapper_summary("artifact-provenance.json", {"status":"FAIL","sourceCommit":expected,"raw":sentinel,
            "failureCode":sentinel,"runtimeIdentity":{"uid":123,"raw":sentinel},"guestInfo":{"imageId":"a"*64,"raw":sentinel},
            "restartObserver":{"raw":sentinel}})
        assert sentinel not in json.dumps(projected) and projected["failureCode"]=="OtherFailure"
        projected = live.project_wrapper_summary("resources.json", {"status":"FAIL","initial":{"memory.current":"1","raw":sentinel},"raw":sentinel})
        assert sentinel not in json.dumps(projected)
    print(f"Protected observer projection self-tests passed; {len(contradictory)} counter/stat contradictions rejected, sample errors>0 retained; raw diagnostics were not exported")
