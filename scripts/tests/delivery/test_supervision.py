"""Synthetic child and observer controls for the live-delivery supervisor."""

import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import tempfile
import time
from unittest.mock import patch

from .test_live_observer import observer_fixture


def supervision_controls(live):
    expected = "1"*40
    def forbidden(*_args,**_kwargs): raise AssertionError("supervisor self-test started business work")
    scenes = ("native-first","observer-first","observer-exit1","native-exit1","early-observer-zero",
              "observer-spawn-failure","native-spawn-failure","interrupt","interrupt-during-native-spawn","native-marker-exit1")
    for scene in scenes:
        with tempfile.TemporaryDirectory(prefix="protected-supervisor-selftest-",dir=live.ARTIFACTS) as directory:
            run = Path(directory); configuration = observer_fixture(live, run,expected)
            terminal = run / "native/public/native-result.json"
            marker = terminal.read_bytes()
            if scene == "early-observer-zero": terminal.unlink()
            tick,children,timeline = [0],{},[]
            handlers = {action:signal.getsignal(action) for action in (signal.SIGINT,signal.SIGTERM)}
            class Child:
                def __init__(self,kind,options):
                    self.kind=kind; self.copies={fd:os.dup(fd) for fd in options["pass_fds"]}; self.forced=False; self.reaped=False
                    self.environment=options["env"]
                    assert all(not os.get_inheritable(fd) for fd in self.copies.values())
                def poll(self):
                    if self.kind=="native":
                        if self.forced or tick[0]>= (3 if scene in ("observer-first","observer-exit1","early-observer-zero","interrupt","interrupt-during-native-spawn") else 1):
                            if not terminal.exists(): terminal.write_bytes(marker)
                            return 1 if self.forced or scene in ("native-exit1","native-marker-exit1") else 0
                    else:
                        done=self.copies[int(self.environment[live.NATIVE_DONE_FD])]
                        eof=bool(select.select([done],[],[],0)[0])
                        if eof: timeline.append(("done-eof",tick[0]))
                        if tick[0]>= (1 if scene in ("observer-first","observer-exit1","early-observer-zero") else 3) or eof and "native" not in children:
                            return 1 if scene=="observer-exit1" or "native" not in children else 0
                    return None
                def wait(self):
                    code=self.poll()
                    assert code is not None, (scene,self.kind,"reaped before actual exit")
                    timeline.append(("wait",self.kind,tick[0])); self.reaped=True
                    for fd in self.copies.values(): os.close(fd)
                    self.copies.clear()
                    return code
                def send_signal(self,signum):
                    assert self.kind=="native", "observer was signalled before its final save"
                    timeline.append(("signal",signum)); self.forced=True
            def spawn(arguments,**options):
                kind="observer" if live.OBSERVER_SCRIPT in arguments[1] else "native"
                if scene==kind+"-spawn-failure": raise OSError("synthetic spawn failure")
                assert not any("--delivery"==part for part in arguments)
                if kind=="observer":
                    assert set(options["pass_fds"])=={int(options["env"][live.OBSERVER_READY_FD]),int(options["env"][live.NATIVE_DONE_FD])}
                else:
                    assert "--protected-config" in arguments and live.NATIVE_DONE_FD not in options["env"]
                    assert options["pass_fds"]==(int(options["env"][live.OBSERVER_READY_FD]),)
                    if scene=="interrupt-during-native-spawn": signal.getsignal(signal.SIGINT)(signal.SIGINT,None)
                children[kind]=Child(kind,options); return children[kind]
            def sleep(_duration):
                tick[0]+=1
                assert tick[0]<12, "supervisor did not converge after child exits"
                if scene=="interrupt" and tick[0]==1:
                    signal.getsignal(signal.SIGINT)(signal.SIGINT,None)
            actual_write=live.write_observer_summary
            def write(*args):
                assert all(child.reaped for child in children.values()), "summary certified a process still running"
                timeline.append(("summary",tick[0])); return actual_write(*args)
            with patch.object(subprocess,"Popen",side_effect=spawn), patch.object(subprocess,"run",forbidden), \
                 patch.object(socket,"socket",forbidden), patch.object(time,"sleep",side_effect=sleep), \
                 patch.object(live, "write_observer_summary", write):
                record,passed=live.supervise_protected(configuration,run,{"TOKIO_WORKER_THREADS":"2"},run / "private.log")
            assert passed == (scene in ("native-first","observer-first")), (scene,record,passed)
            assert all(signal.getsignal(action)==handler for action,handler in handlers.items())
            assert all(child.reaped for child in children.values())
            if scene=="native-first":
                assert record["nativeExitCode"]==record["observerExitCode"]==0
                assert next(item[2] for item in timeline if item[:2]==("wait","native")) < next(item[2] for item in timeline if item[:2]==("wait","observer"))
                assert any(item[0]=="done-eof" for item in timeline)
            if scene in ("observer-exit1","early-observer-zero","interrupt","interrupt-during-native-spawn"):
                assert any(item[0]=="signal" for item in timeline), (scene,timeline)
    print("Protected supervisor self-tests passed; child processes and sockets were mocked")
