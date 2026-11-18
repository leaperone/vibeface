import assert from "node:assert/strict";
import childProcess from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import { setImmediate as tick } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import { runHook } from "../transport.mts";

let calls;
let dir;
const spawn = childProcess.spawn;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "vibeface hook "));
  calls = [];
  childProcess.spawn = (...args) => {
    const child = spawn(...args);
    const closed = new Promise((resolve) => child.once("close", (code, signal) => resolve({ code, signal })));
    calls.push({ args, child, closed });
    return child;
  };
  syncBuiltinESMExports();
});
afterEach(async () => {
  childProcess.spawn = spawn;
  syncBuiltinESMExports();
  for (const { child, closed } of calls) {
    if (child.pid && child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    await closed;
  }
  rmSync(dir, { recursive: true, force: true });
});

const command = (script) => [process.execPath, "-e", script];
const reply = (value) => command(`process.stdin.resume(); process.stdout.write(${JSON.stringify(JSON.stringify(value))});`);

test("UTF-8 split across writes and a legal 1 MiB JSON reply plus newline", async () => {
  const text = "界" + "a".repeat(1024 * 1024 - Buffer.byteLength('{"text":"界"}'));
  const script = `
    process.stdin.resume();
    const bytes = Buffer.from(JSON.stringify({ text: "界" + "a".repeat(${text.length - 1}) }) + "\\n");
    process.stdout.write(bytes.subarray(0, 10));
    setTimeout(() => process.stdout.write(bytes.subarray(10)), 10);
  `;
  const answers = [];
  runHook(command(script), {}, (answer) => answers.push(answer));
  assert.deepEqual(await calls[0].closed, { code: 0, signal: null });
  assert.deepEqual(answers, [{ text }]);
});

for (const [label, argv] of [
  ["invalid JSON", command('process.stdin.resume(); process.stdout.write("{")')],
  ["several JSON values", command('process.stdin.resume(); process.stdout.write("{}{}")')],
  ["empty stdout", command("process.stdin.resume()")],
  ["nonzero exit with complete JSON", command('process.stdin.resume(); process.stdout.write("{}"); process.exitCode = 9')],
  ["signal exit", command('process.stdin.resume(); process.kill(process.pid, "SIGTERM")')],
  ["spawn failure", [join(tmpdir(), "nonexistent-vibeface-hook-" + process.pid)]],
]) {
  test(label + " discards the reply and starts only once", async () => {
    const answers = [];
    runHook(argv, {}, (answer) => answers.push(answer));
    await calls[0].closed;
    assert.deepEqual(answers, []);
    assert.equal(calls.length, 1);
  });
}

test("closed stdin stops the hook before accepting its reply", async () => {
  const argv = command('require("node:fs").closeSync(0); process.stdout.write("{}"); setInterval(() => {}, 1000)');
  const answers = [];
  runHook(argv, { text: "a".repeat(1024 * 1024) }, (answer) => answers.push(answer));
  const error = new Promise((resolve) => calls[0].child.stdin.once("error", resolve));
  assert.ok(["EPIPE", "ECONNRESET"].includes((await error).code));
  assert.equal((await calls[0].closed).signal, "SIGKILL");
  assert.deepEqual(answers, []);
  assert.equal(calls.length, 1);
});

test("stdout beyond the observation limit releases the child and pipes", async () => {
  const argv = command('process.stdin.resume(); process.stdout.write(Buffer.alloc(2 * 1024 * 1024 + 1, 32)); setInterval(() => {}, 1000)');
  const answers = [];
  runHook(argv, {}, (answer) => answers.push(answer));
  const { child, closed } = calls[0];
  assert.equal((await closed).signal, "SIGKILL");
  assert.equal(child.stdin.destroyed && child.stdout.destroyed, true);
  assert.deepEqual(answers, []);
  assert.equal(calls.length, 1);
});

test("the lifetime expires even when stdout contains a complete answer", async (t) => {
  const schedule = globalThis.setTimeout;
  const clear = globalThis.clearTimeout;
  let deadline;
  let cleared = false;
  t.mock.method(globalThis, "setTimeout", (fn, ms, ...args) => {
    if (ms !== 90_000) return schedule(fn, ms, ...args);
    deadline = schedule(fn, 300, ...args);
    return deadline;
  });
  t.mock.method(globalThis, "clearTimeout", (timer) => {
    if (timer === deadline) cleared = true;
    clear(timer);
  });
  const answers = [];
  runHook(command('process.stdin.resume(); process.stdout.write("{}"); setInterval(() => {}, 1000)'), {}, (value) => answers.push(value));
  assert.equal(deadline.hasRef(), false);
  assert.equal((await calls[0].closed).signal, "SIGKILL");
  assert.equal(cleared, true);
  assert.deepEqual(answers, []);
  assert.equal(calls.length, 1);
});

let instance = 0;
async function installed(name, argv) {
  const source = readFileSync(new URL(`../generated/${name}.ts`, import.meta.url), "utf8");
  assert.ok(source.startsWith("// vibeface: "));
  assert.equal(source.split("__VIBEFACE_HOOK__").length, 2);
  const path = join(dir, `${name}-${instance++}.mts`);
  writeFileSync(path, source.replace("__VIBEFACE_HOOK__", () => JSON.stringify(argv)));
  return (await import(pathToFileURL(path).href)).default;
}

function environment(t, key, value) {
  const before = process.env[key];
  if (value === undefined) delete process.env[key];
  else process.env[key] = value;
  t.after(() => {
    if (before === undefined) delete process.env[key];
    else process.env[key] = before;
  });
}

function recordingCommand(answer) {
  const file = join(dir, "stdin.json");
  return command(`
    const chunks = [];
    process.stdin.on("data", chunk => chunks.push(chunk));
    process.stdin.on("end", () => {
      require("node:fs").writeFileSync(${JSON.stringify(file)}, Buffer.concat(chunks));
      process.stdout.write(${JSON.stringify(JSON.stringify(answer))});
    });
  `);
}
function payload() { return JSON.parse(readFileSync(join(dir, "stdin.json"), "utf8")); }

for (const changed of ["none", "session", "leaf"]) {
  test(`Pi settles once and checks the current session and leaf (${changed})`, async (t) => {
    environment(t, "VIBEFACE_PI_RUN", undefined);
    const argv = recordingCommand({ text: " next ➜ " });
    const extension = await installed("pi", argv);
    let handler;
    const sent = [];
    extension({
      on(event, fn) { assert.equal(event, "agent_settled"); handler = fn; },
      sendUserMessage(...args) { sent.push(args); },
    });
    let session = "session";
    let leaf = "leaf";
    const message = { role: "assistant", stopReason: "stop", content: [
      { type: "text", text: "hello" }, { type: "image" }, { type: "text", text: "there" },
    ] };
    const ctx = { cwd: "/my cwd", mode: "tui", sessionManager: {
      getSessionId: () => session, getLeafId: () => leaf,
      getBranch: () => [{ type: "message", message }, { type: "label" }],
    } };
    assert.equal(handler({}, { ...ctx, mode: "rpc" }), undefined);
    assert.equal(calls.length, 0);
    assert.equal(handler({}, ctx), undefined);
    if (changed === "session") session = "new";
    if (changed === "leaf") leaf = "new";
    await calls[0].closed;
    assert.deepEqual(calls[0].args, [argv[0], argv.slice(1), { stdio: ["pipe", "pipe", "ignore"] }]);
    assert.deepEqual(payload(), { session_id: "session", turn_id: "leaf", cwd: "/my cwd", stop_reason: "stop", last_message: "hello\n\nthere" });
    assert.deepEqual(sent, changed === "none" ? [[" next ➜ ", { deliverAs: "followUp" }]] : []);
    assert.equal(calls.length, 1);
  });
}

for (const [label, bytes, expected] of [
  ["invalid UTF-8", Buffer.concat([Buffer.from('{"text":"go'), Buffer.from([0xff]), Buffer.from('now"}')]), []],
  ["literal replacement character", Buffer.from('{"text":"go�now"}'), ["go�now"]],
  ["leading UTF-8 BOM", Buffer.from('\uFEFF{"text":"hello"}'), []],
]) {
  test(`Pi checks hook reply encoding before sending a follow-up: ${label}`, async (t) => {
    environment(t, "VIBEFACE_PI_RUN", undefined);
    const argv = command(`process.stdin.resume(); process.stdout.write(Buffer.from("${bytes.toString("hex")}", "hex"));`);
    const extension = await installed("pi", argv);
    let handler;
    const sent = [];
    extension({ on(_, fn) { handler = fn; }, sendUserMessage(text) { sent.push(text); } });
    handler({}, { mode: "tui", cwd: "/p", sessionManager: {
      getSessionId: () => "s", getLeafId: () => "t",
      getBranch: () => [{ type: "message", message: { role: "assistant", stopReason: "stop" } }],
    } });
    assert.deepEqual(await calls[0].closed, { code: 0, signal: null });
    assert.deepEqual(sent, expected);
    assert.equal(calls.length, 1);
  });
}

test("Pi ignores Node-driven sessions", async (t) => {
  environment(t, "VIBEFACE_PI_RUN", "1");
  (await installed("pi", reply({ text: "unused" })))({ on: assert.fail });
  assert.equal(calls.length, 0);
});

for (const [kind, answer, expected] of [
  ["permission", { reply: "once", message: "ok" }, ["permission", "reply", { reply: "once", message: "ok" }]],
  ["permission", { reply: "always" }, ["permission", "reply", { reply: "always", message: undefined }]],
  ["permission", { reply: "reject", message: "" }, ["permission", "reply", { reply: "reject", message: "" }]],
  ["permission", { reply: "invalid-value", message: { unexpected: true } }, undefined],
  ["permission", { reply: "invalid-value" }, undefined],
  ["permission", { reply: "once", message: { unexpected: true } }, undefined],
  ["permission", { reply: "once", message: null }, undefined],
  ["question", { answers: [["A"]] }, ["question", "reply", { answers: [["A"]] }]],
  ["question", { answers: [["A", "B"], ["C"]] }, ["question", "reply", { answers: [["A", "B"], ["C"]] }]],
  ["question", { answers: [[7]] }, undefined],
  ["question", { answers: ["row"] }, undefined],
  ["question", { answers: [["A"], [null]] }, undefined],
  ["question", { reject: true, answers: [[7]] }, ["question", "reject", undefined]],
  ["question", { reject: true, answers: [["A"]] }, ["question", "reject", undefined]],
  ["permission", { answers: [["A"]] }, undefined],
  ["question", { reply: "once" }, undefined],
]) {
  test(`Opencode ${kind} routes ${JSON.stringify(answer)} on its own client`, async (t) => {
    environment(t, "VIBEFACE_OPENCODE_ENV", undefined);
    const plugin = await installed("opencode", recordingCommand(answer));
    assert.equal(plugin.id, "vibeface");
    const posts = [];
    const client = { _client: { post(input) { posts.push(input); return Promise.reject(new Error("unknown result")); } } };
    const { event } = await plugin.server({ client, directory: "/p" });
    const detail = kind === "permission" ? { permission: "read" } : { questions: [{ header: "choose" }] };
    await event({ event: { type: kind + ".asked", properties: { sessionID: "ses_a", id: "a/b?", ...detail } } });
    await calls[0].closed;
    assert.deepEqual(posts, expected ? [{ url: `/${expected[0]}/a%2Fb%3F/${expected[1]}`, body: expected[2] }] : []);
    assert.deepEqual(payload(), { event: kind + ".asked", session_id: "ses_a", cwd: "/p", request_id: "a/b?", ...detail });
    assert.equal(calls.length, 1);
  });
}

test("Opencode idle looks up a root session before reading its ten messages", async (t) => {
  environment(t, "VIBEFACE_OPENCODE_ENV", undefined);
  const plugin = await installed("opencode", recordingCommand({}));
  const requests = [];
  let session = { parentID: "other" };
  const client = { session: {
    async get(input) { requests.push(["get", input]); return { data: session }; },
    async messages(input) {
      requests.push(["messages", input]);
      return { data: [{ info: { role: "assistant", id: "turn", error: { name: "MessageAbortedError" } },
        parts: [{ type: "text", text: "visible" }, { type: "text", synthetic: true, text: "hidden" }] }] };
    },
  } };
  const { event } = await plugin.server({ client, directory: "/p" });
  const idle = { event: { type: "session.idle", properties: { sessionID: "ses_a" } } };
  await event(idle);
  await tick();
  assert.equal(calls.length, 0);
  session = {};
  await event(idle);
  await tick();
  await calls[0].closed;
  const path = { id: "ses_a" };
  assert.deepEqual(requests, [["get", { path }], ["get", { path }], ["messages", { path, query: { limit: 10 } }]]);
  assert.deepEqual(payload(), { event: "session.idle", session_id: "ses_a", cwd: "/p", turn_id: "turn",
    status: "interrupted", error: "MessageAbortedError", last_message: "visible" });
});

test("Opencode enables only the serve shell handler with a configured environment directory", async (t) => {
  environment(t, "VIBEFACE_OPENCODE_ENV", undefined);
  const args = { client: {}, directory: "/p" };
  assert.deepEqual(await (await installed("opencode", null)).server(args), {});
  process.env.VIBEFACE_OPENCODE_ENV = dir;
  assert.deepEqual(await (await installed("opencode", reply({}))).server(args), {});
  const plugin = await installed("opencode", null);
  assert.equal(plugin.id, "vibeface-serve");
  const hooks = await plugin.server(args);
  assert.deepEqual(Object.keys(hooks), ["shell.env"]);
  writeFileSync(join(dir, "default.json"), JSON.stringify({ SETTING: "", BASE: "yes" }));
  writeFileSync(join(dir, "ses_a.json"), JSON.stringify({ SETTING: "allowed" }));
  for (const [sessionID, setting] of [["ses_a", "allowed"], ["../ses_a", ""]]) {
    const output = { env: { KEEP: "value", SETTING: "old" } };
    await hooks["shell.env"]({ sessionID }, output);
    assert.deepEqual(output.env, { KEEP: "value", SETTING: setting, BASE: "yes" });
  }
  writeFileSync(join(dir, "ses_a.json"), "{");
  const fallback = { env: { SETTING: "old" } };
  await hooks["shell.env"]({ sessionID: "ses_a" }, fallback);
  assert.deepEqual(fallback.env, { SETTING: "", BASE: "yes" });
  await assert.rejects(hooks["shell.env"]({}, { env: Object.freeze({}) }), TypeError);
  assert.equal(calls.length, 0);
});
