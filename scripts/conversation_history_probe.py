"""Conversation paging acceptance through the existing isolated Core process fixture."""
import hashlib
import json
from pathlib import Path
import sqlite3
import subprocess
import uuid


HISTORY_BYTES = 3 * 1024 * 1024


def encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()


def conversation_history_acceptance(options, harness):
    repository = Path(harness.__file__).resolve().parents[1]
    artifacts = options.artifacts.resolve()
    harness.require(artifacts.is_relative_to(repository / ".artifacts"),
                    "history process fixtures stay under the task worktree .artifacts")
    binary = options.binary.resolve(strict=True)
    core = options.core_binary.resolve(strict=True)
    module = options.crypto_module.resolve(strict=True)
    artifacts.mkdir(parents=True, exist_ok=True)
    run = artifacts / ("conversation-history-" + uuid.uuid4().hex[:10])
    run.mkdir(mode=0o700)
    workspace = run / "workspace"
    workspace.mkdir(mode=0o700)
    helper = run / "client.mjs"
    helper.write_text(harness.CLIENT_SOURCE.replace("MODULE_URL", module.as_uri()))
    guard = run / "provider-guard"
    provider_calls = run / "provider-invocations.txt"
    guard.write_text("#!/usr/bin/env python3\nfrom pathlib import Path\n"
                     f"with Path({str(provider_calls)!r}).open('a') as output: output.write('invoked\\n')\n"
                     "raise SystemExit(97)\n")
    guard.chmod(0o700)
    report = {"status": "running", "source_commit": subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repository, text=True).strip(),
        "binary_sha256": harness.file_sha256(binary), "core_sha256": harness.file_sha256(core),
        "provider_requests": 0, "messages": 1024, "pages": 0, "max_response_bytes": 0,
        "coverage": "real Core process and authenticated encrypted client; no browser rendering",
        "checks": [], "cleanup": False}
    node = None
    processes = []
    clients = []

    def checked(name):
        report["checks"].append(name)
        print("PASS conversation-history " + name, flush=True)

    def bounded(value):
        size = len(encoded(value))
        harness.require(size < HISTORY_BYTES, "the complete serialized response stays below 3 MiB")
        report["max_response_bytes"] = max(report["max_response_bytes"], size)
        return value

    def page(cursor):
        value = bounded(node.client.rpc("conversation_history", conversation_id="history",
                                       section="messages", cursor=cursor, limit=100))
        harness.require(value["conversation_id"] == "history" and value["section"] == "messages",
                        "history response is bound to its requested conversation and section")
        harness.require(value["complete"] == (value["next_cursor"] is None), "continuation is explicit")
        report["pages"] += 1
        return value

    try:
        node = harness.Node(run, "HISTORY", binary, workspace, helper)
        node.configure(manager=None, bins={name: str(guard) for name in ("codex", "claude", "cursor", "opencode", "pi")},
                       app_server=harness.SPAWN_APP_SERVER)
        harness.isolate_delivery_node(node)
        node.start()
        processes.append(node.process)
        network = node.bootstrap()
        for name in ("history", "other-history"):
            node.client.rpc("create_bot", bot_id=name, conversation_id=name, name=name,
                            instructions="", project=None)
        expected = [(f"history-{index:04}", f"{index}:" + "完整正文\n" * 1024) for index in range(1024)]
        harness.require(len(encoded(expected)) > 4 * 1024 * 1024, "the signed corpus exceeds the old full-response budget")
        for index, (message_id, text) in enumerate(expected):
            node.client.rpc("message", conversation_id="history", message_id=message_id, text=text,
                            intent="defer", report_results=False)
            if (index + 1) % 128 == 0:
                print(f"conversation-history signed inputs {index + 1}/1024", flush=True)
        snapshot = bounded(node.client.rpc("conversation", conversation_id="history"))
        initial_cursor = snapshot["history"]["messages"]["next_cursor"]
        harness.require(initial_cursor is not None and len(snapshot["messages"]) < len(expected),
                        "initial conversation is a recent window with a real older-history cursor")
        authority = snapshot["history_authority"]
        checked("bounded-default-snapshot")
        node.client.rpc("conversation_history", conversation_id="other-history", section="messages",
                        cursor=initial_cursor, status=400)
        node.client.rpc("conversation_history", conversation_id="history", section="works",
                        cursor=initial_cursor, status=400)
        checked("wrong-conversation-and-section-rejected")
        before_restart = page(initial_cursor)
        clients.append(node.client.process)
        node.client.close()
        node.stop()
        node.start()
        processes.append(node.process)
        node.client.connect(node.device_id)
        node.client.authenticate(network)
        harness.require(page(initial_cursor) == before_restart, "restarted Core reads the same durable cursor and exact rows")
        checked("process-restart-and-client-reconnect")
        tail = ("history-new-tail", "new tail after the first history page")
        node.client.rpc("message", conversation_id="history", message_id=tail[0], text=tail[1],
                        intent="defer", report_results=False)
        rows = list(snapshot["messages"])
        cursor = initial_cursor
        seen_cursors = set()
        while cursor is not None:
            harness.require(cursor not in seen_cursors, "history traversal makes progress")
            seen_cursors.add(cursor)
            history = page(cursor)
            harness.require(history["history_authority"] == authority, "ordinary tail arrival retains the history authority")
            rows[0:0] = history["items"]
            cursor = history["next_cursor"]
        actual = [(row["message_id"], row["text"]) for row in rows]
        harness.require(actual == expected, "every original full text and message ID is recovered once in shared order")
        harness.require(len({row[0] for row in actual}) == len(expected), "pages never duplicate stable message IDs")
        checked("full-history-content-order-and-no-duplicates")
        latest = bounded(node.client.rpc("conversation", conversation_id="history"))
        harness.require(latest["history_authority"] == authority, "ordinary refresh preserves the loaded projection")
        merged = {row["message_id"]: row for row in rows}
        merged.update((row["message_id"], row) for row in latest["messages"])
        merged_rows = sorted(merged.values(), key=lambda row: (row["rank"], row["message_id"]))
        harness.require([(row["message_id"], row["text"]) for row in merged_rows] == expected + [tail],
                        "the latest window merges with loaded history without losing the new tail")
        with sqlite3.connect((node.directory / "node.sqlite3").as_uri() + "?mode=ro", uri=True) as database:
            oracle = database.execute("SELECT message_id,text FROM messages WHERE network_id=? AND conversation_id=? ORDER BY rank,message_id",
                                      (network, "history")).fetchall()
            pending = database.execute("SELECT count(*) FROM scheduler_jobs").fetchone()[0]
        harness.require(oracle == expected + [tail], "encrypted RPC pages match the real process database oracle")
        harness.require(pending == 0, "deferred inputs never admit provider work")
        checked("new-arrival-merge-and-readonly-database-oracle")
        harness.verify_dag(node, network)
        report["content_sha256"] = hashlib.sha256(encoded(expected + [tail])).hexdigest()
        report["status"] = "passed"
    except Exception:
        report["status"] = "failed"
        raise
    finally:
        cleanup_errors = []
        if node is not None:
            if node.client.process is not None:
                clients.append(node.client.process)
            if node.process is not None and node.process not in processes:
                processes.append(node.process)
            for close in (node.client.close, node.stop):
                try:
                    close()
                except Exception as error:
                    cleanup_errors.append(type(error).__name__)
        report["cleanup_errors"] = cleanup_errors
        report["provider_requests"] = len(provider_calls.read_text().splitlines()) if provider_calls.exists() else 0
        report["cleanup"] = not cleanup_errors and all(process.poll() is not None for process in processes + clients)
        if report["provider_requests"] != 0 or not report["cleanup"]:
            report["status"] = "failed"
        harness.private_json(run / "summary.json", report)
        print("conversation-history report: " + str(run / "summary.json"), flush=True)
    harness.require(report["provider_requests"] == 0, "no Provider program was invoked")
    harness.require(report["cleanup"], "all task-owned Core process groups stopped")
