import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { createContext, SourceTextModule, SyntheticModule } from "node:vm";

async function extension(env, spawn) {
  const context = createContext({
    process: { env },
    __VIBEFACE_HOOK__: ["fixture-vibeface", "hook", "pi"],
  });
  const childProcess = new SyntheticModule(
    ["spawn"],
    function () {
      this.setExport("spawn", spawn);
    },
    { context },
  );
  const entry = new SourceTextModule(
    await readFile(new URL("./pi.ts", import.meta.url), "utf8"),
    { context },
  );
  await entry.link((specifier) => {
    assert.equal(specifier, "node:child_process");
    return childProcess;
  });
  await entry.evaluate();
  return entry.namespace.default;
}

test("Node-managed runs stay unwatched while user runs report settled turns", async () => {
  const hook = new EventEmitter();
  hook.stdout = new EventEmitter();
  hook.stdout.setEncoding = () => {};
  hook.stdin = new EventEmitter();
  const reports = [];
  const launches = [];
  hook.stdin.end = (body) => reports.push(JSON.parse(body));
  const spawn = (program, args, options) => {
    launches.push(JSON.parse(JSON.stringify({ program, args, options })));
    return hook;
  };
  const managedEvents = [];
  const managed = await extension({ VIBEFACE_PI_RUN: "1" }, () => {
    assert.fail("a Node-managed run must not start a hook");
  });
  managed({ on: (name) => managedEvents.push(name) });
  assert.deepEqual(managedEvents, []);

  const handlers = new Map();
  const replies = [];
  const pi = {
    on: (name, handler) => {
      assert.equal(handlers.has(name), false, "the handler must be registered once");
      handlers.set(name, handler);
    },
    sendUserMessage: (text, options) => replies.push({ text, ...options }),
  };
  const ordinary = await extension({}, spawn);
  ordinary(pi);
  assert.deepEqual([...handlers.keys()], ["agent_settled"]);
  handlers.get("agent_settled")({}, {
    mode: "tui",
    cwd: "/fixture/project",
    sessionManager: {
      getSessionId: () => "session-1",
      getLeafId: () => "turn-1",
      getBranch: () => [{
        type: "message",
        message: {
          role: "assistant",
          stopReason: "stop",
          content: [{ type: "text", text: "Done" }],
        },
      }],
    },
  });
  assert.deepEqual(launches, [{
    program: "fixture-vibeface",
    args: ["hook", "pi"],
    options: { stdio: ["pipe", "pipe", "ignore"] },
  }]);
  assert.deepEqual(reports, [{
    session_id: "session-1",
    turn_id: "turn-1",
    cwd: "/fixture/project",
    stop_reason: "stop",
    last_message: "Done",
  }]);
  hook.stdout.emit("data", '{"text":"Continue with the tests"}');
  hook.emit("close");
  assert.deepEqual(replies, [{ text: "Continue with the tests", deliverAs: "followUp" }]);
});
