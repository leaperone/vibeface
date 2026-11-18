import binascii
import json
from pathlib import Path
import runpy
import sys
import tempfile
import unittest

here = Path(__file__).resolve().parent
sys.path.insert(0, str(here.parents[1] / "scripts"))
verify = runpy.run_path(str(here.parents[1] / "scripts/verify-node.py"))


class RequestContract(unittest.TestCase):
    def test_shared_corpus(self):
        for kind, surfaces, request in json.loads((here / "fixture-requests.json").read_text()):
            with self.subTest(kind=kind, request=request):
                if "child" in surfaces.split():
                    self.assertEqual(verify["validate_fixture_request"](request, kind), request["op"])
                else:
                    with self.assertRaises((AssertionError, ValueError, binascii.Error)):
                        verify["validate_fixture_request"](request, kind)


def workspace_example():
    class NativeClient:
        def rpc(self, method):
            assert method == "workspace_state"
            return {"inputs": [{"input": {"message_id": "m", "target": {"kind": "task", "task_id": "a"}, "text": ""},
                                "conversation_id": "chat", "deferred": True, "cancelled": False}]}

    with tempfile.TemporaryDirectory() as temporary:
        run = Path(temporary)
        verify["fixture_record"](run, "manager-invocations.jsonl", {"label": "a", "conversation_id": "chat",
                                "trigger": "message:m", "native_session_id": "thread", "messages": ["hello"]})
        verify["fixture_record"](run, "codex-inputs.jsonl", {"event": "turn_started", "thread_id": "thread", "turn_id": "turn"})
        verify["fixture_record"](run, "codex-inputs.jsonl", {"event": "input", "thread_id": "thread", "turn_id": "turn",
                                "message_id": "m", "input": [{"type": "text", "text": "hello"}]})
        verify["fixture_record"](run, "codex-rpc.jsonl", {"message": {"method": "turn/interrupt",
                                "params": {"threadId": "thread", "turnId": "turn"}}})
        (run / "fixture-codex-state.json").write_text(json.dumps({"threads": {"thread": {"id": "thread",
            "turns": [{"id": "turn", "status": "inProgress"}]}}}))
        print(json.dumps([verify["workspace_fixture_evidence"](run, client) for client in (None, NativeClient())]))


if __name__ == "__main__":
    if sys.argv[1:] == ["--workspace-example"]:
        workspace_example()
    else:
        unittest.main()
