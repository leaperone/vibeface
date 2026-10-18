#!/usr/bin/env node
import childProcess from "node:child_process";
import { access, chmod, constants, stat, unlink } from "node:fs/promises";
import { createConnection, createServer } from "node:net";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { syncBuiltinESMExports } from "node:module";
import { setTimeout as delay } from "node:timers/promises";
import path from "node:path";
import { Codex } from "@openai/codex-sdk";

const MAX_FRAME_BYTES = 1_048_576;
const MAX_PROMPT_BYTES = 65_536;
const MAX_RESULT_BYTES = 65_536;
const MAX_EVENT_BYTES = 65_536;
const MAX_STREAM_BYTES = 16_777_216;
const MAX_ID_BYTES = 256;
const MAX_MODEL_BYTES = 256;
const ID_PATTERN = /^[^\u0000-\u001f\u007f\r\n]{1,256}$/;
const INIT_FIELDS = new Set([
  "type",
  "prompt",
  "session_id",
  "cwd",
  "model",
  "codex_path",
  "tools",
]);

class InputClosedError extends Error {}

class ProtocolError extends Error {
  constructor(message) {
    super(message);
    this.name = "ProtocolError";
  }
}

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function bytes(value) {
  return Buffer.byteLength(value, "utf8");
}

function requireString(value, name, maxBytes) {
  if (typeof value !== "string" || value.length === 0 || bytes(value) > maxBytes) {
    throw new ProtocolError(`${name} must be a non-empty string of at most ${maxBytes} bytes`);
  }
  return value;
}

function requireId(value, name = "session_id") {
  const id = requireString(value, name, MAX_ID_BYTES);
  if (!ID_PATTERN.test(id)) {
    throw new ProtocolError(`${name} contains control characters`);
  }
  return id;
}

function requireAbsolutePath(value, name) {
  const result = requireString(value, name, 4_096);
  if (!path.isAbsolute(result) || result.includes("\0")) {
    throw new ProtocolError(`${name} must be an absolute path`);
  }
  return result;
}

function rejectUnknownFields(value, allowed, name) {
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) {
      throw new ProtocolError(`${name} contains unsupported field ${key}`);
    }
  }
}

async function validateExecutable(value, name) {
  const executable = requireAbsolutePath(value, name);
  const metadata = await stat(executable).catch(() => null);
  if (!metadata?.isFile()) {
    throw new ProtocolError(`${name} is not a regular file`);
  }
  await access(executable, constants.X_OK).catch(() => {
    throw new ProtocolError(`${name} is not executable`);
  });
  return executable;
}

async function validateCwd(value) {
  const cwd = requireAbsolutePath(value, "cwd");
  const metadata = await stat(cwd).catch(() => null);
  if (!metadata?.isDirectory()) {
    throw new ProtocolError("cwd must be an existing directory");
  }
  return cwd;
}

function validateInit(value) {
  if (!isObject(value) || value.type !== "init") {
    throw new ProtocolError("first frame must be an init request");
  }
  rejectUnknownFields(value, INIT_FIELDS, "init");
  const prompt = requireString(value.prompt, "prompt", MAX_PROMPT_BYTES);
  const sessionId = value.session_id === undefined || value.session_id === null || value.session_id === ""
    ? null
    : requireId(value.session_id);
  const model = value.model === undefined || value.model === null || value.model === ""
    ? undefined
    : requireString(value.model, "model", MAX_MODEL_BYTES);
  return {
    prompt,
    sessionId,
    cwd: value.cwd,
    model,
    codexPath: value.codex_path,
    tools: validateTools(value.tools),
  };
}

function validateTools(value) {
  if (!isObject(value) || Object.keys(value).length > 32 || bytes(JSON.stringify(value)) > MAX_EVENT_BYTES) throw new ProtocolError("tools must be a bounded tool catalog");
  for (const [name, spec] of Object.entries(value)) {
    if (!/^[a-z][a-z0-9_]{0,127}$/.test(name) || !isObject(spec) || !isObject(spec.inputSchema) || spec.inputSchema.type !== "object") throw new ProtocolError("invalid custom tool schema");
    if (spec.description !== undefined && typeof spec.description !== "string") throw new ProtocolError("invalid custom tool description");
  }
  return value;
}

function redactString(text, secrets) {
  let result = text;
  for (const secret of secrets) {
    if (secret.length >= 8) result = result.split(secret).join("[redacted]");
  }
  return result;
}

function redact(value, secrets) {
  if (typeof value === "string") return redactString(value, secrets);
  if (Array.isArray(value)) return value.map((item) => redact(item, secrets));
  if (isObject(value)) {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, redact(item, secrets)]));
  }
  return value;
}

function cap(value, limit) {
  if (typeof value === "string") {
    return bytes(value) <= limit ? value : `${value.slice(0, Math.max(0, limit - 32))}...[truncated]`;
  }
  if (Array.isArray(value)) return value.map((item) => cap(item, limit));
  if (isObject(value)) return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, cap(item, limit)]));
  return value;
}

function bounded(value, limit) {
  const candidate = cap(value, limit);
  if (bytes(JSON.stringify(candidate)) <= limit) return candidate;
  return { type: "truncated", original_type: value?.type ?? "unknown" };
}

function write(value) {
  const frame = JSON.stringify(value);
  if (bytes(frame) > MAX_FRAME_BYTES) throw new ProtocolError("sidecar output frame exceeded the size limit");
  process.stdout.write(`${frame}\n`);
}

function closeInput() {
  if (!process.stdin.destroyed) process.stdin.destroy();
}

function sdkConfig() {
  return {
    approval_policy: "never",
    web_search: "disabled",
    check_for_update_on_startup: false,
    analytics: { enabled: false },
    feedback: { enabled: false },
    include_apps_instructions: false,
    include_collaboration_mode_instructions: false,
    include_environment_context: false,
    include_environment_context_time: false,
    include_permissions_instructions: false,
    shell_environment_policy: {
      inherit: "all",
      ignore_default_excludes: true,
      include_only: ["HOME", "PATH", "TMPDIR", "LANG", "LC_ALL", "VIBEFACE_MANAGER_TOOL_SOCKET"],
    },
    mcp_servers: {},
    features: {
      shell_tool: true,
      unified_exec: true,
      unified_exec_tty: false,
      stable_environment_tools: false,
      view_image: false,
      sleep_tool: false,
      request_permissions_tool: false,
      web_search_request: false,
      web_search_cached: false,
      standalone_web_search: false,
      code_mode: true,
      code_mode_host: true,
      code_mode_only: false,
      code_mode_only_strict_3p_tools: false,
      multi_agent: false,
      multi_agent_v2: false,
      multi_agent_v2_dynamic_tools: false,
      collab: false,
      apps: false,
      plugins: false,
      enable_mcp_apps: false,
      mcp_20260728: false,
      codex_apps_mcp_20260728: false,
      tool_suggest: false,
      skip_host_skill_discovery: true,
      apply_patch_freeform: false,
    },
  };
}

function childEnvironment(socket) {
  const env = {};
  for (const key of ["CODEX_HOME", "HOME", "PATH", "LANG", "LC_ALL", "TMPDIR", "OPENAI_API_KEY", "OPENAI_BASE_URL"]) {
    if (process.env[key] !== undefined) env[key] = process.env[key];
  }
  if (socket !== undefined) env.VIBEFACE_MANAGER_TOOL_SOCKET = socket;
  return env;
}

function managerPrompt(prompt, toolCli, tools) {
  if (toolCli === undefined) return prompt;
  const executable = `'${toolCli.replaceAll("'", "'\"'\"'")}'`;
  const catalog = Object.entries(tools).map(([name, spec]) => `${name}\n${spec.description || ""}\nJSON schema: ${JSON.stringify(spec.inputSchema)}`).join("\n\n");
  const result = `Node tools are available through your command-execution tool. Run:\n${executable} tool TOOL_NAME --json 'JSON_OBJECT'\nFor every write tool, add --call-ref 'UNIQUE_INTENT_REFERENCE'. Choose the reference once before the first request and preserve it across CLI processes. Reuse that reference with identical arguments after a partial response. On a timeout, query call_lookup with {"call_ref":"THE_SAME_REFERENCE"}; never replace an uncertain call with a new reference. Different authorized intentions need different references, even when their command text is equal. Returned work_ref values select existing work without exposing SDK session bookkeeping.\nThe CLI uses a private local tool channel and prints JSON. Node callback credentials remain outside this process. Use only the listed Node tools for execution; do not edit project files or launch executors directly. Report failed calls honestly. Conversation and Space scope come from Node.\n\nAvailable tools for this invocation:\n${catalog}\n\n${prompt}`;
  if (bytes(result) > MAX_PROMPT_BYTES) throw new ProtocolError("management prompt with Node tool instructions exceeded the size limit");
  return result;
}

async function* frameReader(stream) {
  let pending = Buffer.alloc(0);
  for await (const chunk of stream) {
    pending = Buffer.concat([pending, chunk]);
    if (pending.length > MAX_FRAME_BYTES + 1) {
      throw new ProtocolError("sidecar input frame exceeded the size limit");
    }
    let newline;
    while ((newline = pending.indexOf(0x0a)) >= 0) {
      const line = pending.subarray(0, newline);
      pending = pending.subarray(newline + 1);
      if (line.length > MAX_FRAME_BYTES) {
        throw new ProtocolError("sidecar input frame exceeded the size limit");
      }
      yield line.toString("utf8");
    }
  }
  if (pending.length > 0) throw new ProtocolError("sidecar input closed before a complete frame");
}

async function readFrame(reader) {
  const line = await reader.next();
  if (line.done) throw new InputClosedError("sidecar input closed before the next frame");
  try {
    return JSON.parse(line.value);
  } catch {
    throw new ProtocolError("sidecar input was not valid JSON");
  }
}

async function toolCommand(args) {
  const name = args[0];
  if (typeof name !== "string" || !/^[a-z][a-z0-9_]{0,127}$/.test(name)) throw new ProtocolError("invalid tool command");
  const options = new Map();
  for (let i = 1; i < args.length; i += 2) {
    if (!["--json", "--call-ref"].includes(args[i]) || options.has(args[i]) || args[i + 1] === undefined) throw new ProtocolError("invalid tool command");
    options.set(args[i], args[i + 1]);
  }
  const encodedArguments = options.get("--json");
  if (encodedArguments === undefined || bytes(encodedArguments) > MAX_PROMPT_BYTES) throw new ProtocolError("invalid tool arguments");
  const argumentsValue = JSON.parse(encodedArguments);
  if (!isObject(argumentsValue)) throw new ProtocolError("invalid tool arguments");
  const argumentRef = argumentsValue.call_ref;
  const flagRef = options.get("--call-ref");
  if (argumentRef !== undefined) requireId(argumentRef, "arguments.call_ref");
  if (flagRef !== undefined) requireId(flagRef, "--call-ref");
  if (argumentRef !== undefined && flagRef !== undefined && argumentRef !== flagRef) throw new ProtocolError("--call-ref must match arguments.call_ref");
  const callRef = flagRef ?? argumentRef;
  if (!["nodes", "context", "execution_status", "call_lookup"].includes(name) && callRef === undefined) throw new ProtocolError("write tools require --call-ref; preserve it and query call_lookup after an uncertain result");
  if (callRef !== undefined) argumentsValue.call_ref = callRef;
  const socketPath = process.env.VIBEFACE_MANAGER_TOOL_SOCKET;
  if (typeof socketPath !== "string" || !socketPath.startsWith("/tmp/vibeface-manager-tools-")) throw new ProtocolError("private tool channel missing");
  const socket = createConnection(socketPath);
  socket.setTimeout(10_000, () => socket.destroy(new Error("tool channel timeout")));
  try {
    await new Promise((resolve, reject) => { socket.once("connect", resolve); socket.once("error", reject); });
    socket.write(`${JSON.stringify({ name, arguments: argumentsValue })}\n`);
    const result = await readFrame(frameReader(socket));
    process.stdout.write(`${JSON.stringify(result)}\n`);
  } finally { socket.destroy(); }
}

async function startToolChannel(catalog) {
  const pending = new Map();
  if (Object.keys(catalog).length === 0) return { path: undefined, respond() { throw new ProtocolError("unexpected tool response"); }, close() {} };
  const socketPath = `/tmp/vibeface-manager-tools-${process.pid}-${randomUUID()}.sock`;
  let closed = false;
  let connections = 0;
  const server = createServer((socket) => {
    if (connections >= 16) { socket.destroy(); return; }
    connections += 1;
    socket.setTimeout(10_000, () => socket.destroy());
    socket.once("close", () => { connections -= 1; });
    void (async () => {
      const request = await readFrame(frameReader(socket));
      rejectUnknownFields(request, new Set(["name", "arguments"]), "tool request");
      if (closed || !Object.hasOwn(catalog, request.name) || !isObject(request.arguments) || pending.size >= 16) throw new ProtocolError("tool outside catalog");
      const callId = randomUUID();
      const timeout = setTimeout(() => { pending.delete(callId); socket.destroy(); }, 10_000);
      socket.once("close", () => { clearTimeout(timeout); pending.delete(callId); });
      pending.set(callId, { socket, timeout });
      write({ type: "tool_call", call_id: callId, name: request.name, arguments: request.arguments });
    })().catch(() => socket.destroy());
  });
  await new Promise((resolve, reject) => { server.once("error", reject); server.listen(socketPath, resolve); });
  await chmod(socketPath, 0o600);
  return {
    path: socketPath,
    respond(id, result) {
      const request = pending.get(id);
      if (!request) throw new ProtocolError("tool response has no pending request");
      pending.delete(id);
      clearTimeout(request.timeout);
      const frame = JSON.stringify(result);
      if (bytes(frame) > MAX_FRAME_BYTES) throw new ProtocolError("tool result too large");
      request.socket.end(`${frame}\n`);
    },
    close() {
      if (closed) return;
      closed = true;
      for (const request of pending.values()) { clearTimeout(request.timeout); request.socket.destroy(); }
      pending.clear();
      server.close();
      void unlink(socketPath).catch(() => {});
    },
  };
}

function startControlPump(reader, controller, tools, sessionId) {
  let pendingSessionAck = null;
  const queuedSessionAcks = [];
  let closed = false;
  let cancelled = false;
  let failure = null;
  const rejectPending = (error) => {
    if (pendingSessionAck) {
      pendingSessionAck.reject(error);
      pendingSessionAck = null;
    }
  };
  const task = (async () => {
    try {
      while (true) {
        const frame = await readFrame(reader);
        if (isObject(frame) && frame.type === "session_ack" && Object.keys(frame).every((key) => ["type", "session_id"].includes(key))) {
          if (!pendingSessionAck || frame.session_id !== pendingSessionAck.id) {
            if (queuedSessionAcks.length >= 4) throw new ProtocolError("too many queued session ACKs");
            queuedSessionAcks.push(frame.session_id);
            continue;
          }
          pendingSessionAck.resolve();
          pendingSessionAck = null;
          continue;
        }
        if (isObject(frame) && frame.type === "tool_result") {
          rejectUnknownFields(frame, new Set(["type", "call_id", "result"]), "tool result");
          tools.respond(requireId(frame.call_id, "call_id"), frame.result);
          continue;
        }
        if (isObject(frame) && frame.type === "cancel" && Object.keys(frame).every((key) => ["type", "session_id"].includes(key))) {
          if ((frame.session_id ?? null) !== sessionId()) throw new ProtocolError("cancel did not match the native session");
          cancelled = true;
          throw new ProtocolError("Codex execution cancelled");
        }
        throw new ProtocolError("unexpected control frame");
      }
    } catch (error) {
      closed = true;
      cancelled ||= error instanceof InputClosedError;
      failure = error;
      rejectPending(error);
      controller.abort();
      tools.close();
    }
  })();
  return {
    task,
    get cancelled() { return cancelled; },
    get failure() { return failure; },
    waitSessionAck(id) {
      if (closed || pendingSessionAck) return Promise.reject(new ProtocolError("session ACK channel is unavailable"));
      const queued = queuedSessionAcks.indexOf(id);
      if (queued >= 0) {
        queuedSessionAcks.splice(queued, 1);
        return Promise.resolve();
      }
      return new Promise((resolve, reject) => {
        pendingSessionAck = { id, resolve, reject };
      });
    },
  };
}

function manageCodexProcess(executable, signal) {
  const originalSpawn = childProcess.spawn;
  let child = null;
  let closed = false;
  let stopping = null;
  childProcess.spawn = function (file, args, options) {
    if (file !== executable) return originalSpawn(file, args, options);
    if (signal.aborted) throw new ProtocolError("Codex execution cancelled before spawn");
    if (child) throw new ProtocolError("Codex SDK attempted more than one managed process");
    child = originalSpawn(file, args, { ...options, detached: process.platform !== "win32" });
    const observeClose = () => { closed = true; };
    const observeError = () => {};
    child.once("close", observeClose);
    child.on("error", observeError);
    const removeAllListeners = child.removeAllListeners;
    child.removeAllListeners = function (event) {
      const result = event === undefined ? removeAllListeners.call(this) : removeAllListeners.call(this, event);
      // The pinned SDK removes all listeners before the child necessarily emits close.
      if (!closed && (event === undefined || event === "close")) this.once("close", observeClose);
      if (event === undefined || event === "error") this.on("error", observeError);
      return result;
    };
    return child;
  };
  syncBuiltinESMExports();
  const groupExists = () => {
    try {
      process.kill(-child.pid, 0);
      return true;
    } catch (error) {
      return error.code !== "ESRCH";
    }
  };
  const killGroup = (name) => {
    try { process.kill(-child.pid, name); } catch {}
  };
  const stop = () => stopping ??= (async () => {
    if (!child) return true;
    if (child.pid && process.platform !== "win32") {
      // shortcut: Codex tools create separate groups; CLI-group exit cannot confirm tool exit without OS containment.
      killGroup("SIGTERM");
      const started = performance.now();
      while (performance.now() - started < 1_500) {
        if (closed && !groupExists()) return true;
        if (performance.now() - started >= 300) killGroup("SIGKILL");
        await delay(20);
      }
    } else {
      if (process.platform === "win32" && !closed) {
        try { child.kill("SIGKILL"); } catch {}
      }
      for (let attempt = 0; attempt < 75; attempt++) {
        if (closed) return true;
        await delay(20);
      }
    }
    // An unconfirmed child must not keep the sidecar alive after its uncertain response.
    try { child.kill("SIGKILL"); } catch {}
    for (const stream of child.stdio) stream?.destroy();
    child.unref();
    return false;
  })();
  signal.addEventListener("abort", stop, { once: true });
  return {
    stop,
    restore() {
      signal.removeEventListener("abort", stop);
      childProcess.spawn = originalSpawn;
      syncBuiltinESMExports();
    },
  };
}

async function run(input, reader, secrets) {
  const cwd = await validateCwd(input.cwd);
  const codexPath = await validateExecutable(input.codexPath, "codex_path");
  const toolCli = Object.keys(input.tools).length === 0 ? undefined : fileURLToPath(import.meta.url);
  const tools = await startToolChannel(input.tools);
  try {
    const prompt = managerPrompt(input.prompt, toolCli, input.tools);

    const codex = new Codex({
      codexPathOverride: codexPath,
      config: sdkConfig(),
      baseUrl: process.env.OPENAI_BASE_URL,
      apiKey: process.env.OPENAI_API_KEY,
      env: childEnvironment(tools.path),
    });
    const threadOptions = {
      model: input.model,
      threadSource: "vibeface-manager",
      workingDirectory: cwd,
      skipGitRepoCheck: true,
      approvalPolicy: "never",
      sandboxMode: "danger-full-access",
      networkAccessEnabled: false,
      webSearchMode: "disabled",
    };
    const thread = input.sessionId === null
      ? codex.startThread(threadOptions)
      : codex.resumeThread(input.sessionId, threadOptions);

    write({ type: "ready", session_id: input.sessionId, protocol: 1 });
    const ack = await readFrame(reader);
    if (!isObject(ack) || ack.type !== "ack" || (ack.session_id ?? null) !== input.sessionId || Object.keys(ack).some((key) => !["type", "session_id"].includes(key))) {
      throw new ProtocolError("expected an ACK for the confirmed session");
    }

    const controller = new AbortController();
    let signalCancelled = false;
    let nativeSessionId = input.sessionId;
    const onSignal = () => { signalCancelled = true; controller.abort(); };
    process.once("SIGTERM", onSignal);
    process.once("SIGINT", onSignal);
    const managed = manageCodexProcess(codexPath, controller.signal);
    const control = startControlPump(reader, controller, tools, () => nativeSessionId);
    const interrupted = new Promise((_, reject) => {
      controller.signal.addEventListener("abort", () => {
        managed.stop().then(() => reject(new ProtocolError("Codex execution aborted")));
      }, { once: true });
    });
    let finalResponse = "";
    let usage = null;
    let turnCompleted = false;
    let streamBytes = 0;
    let failure = null;
    try {
      await Promise.race([(async () => {
        const streamed = await thread.runStreamed(prompt, { signal: controller.signal });
        for await (const rawEvent of streamed.events) {
          const event = redact(rawEvent, secrets);
          if (event?.type === "thread.started") {
            const observed = requireId(event.thread_id, "thread_id");
            if (nativeSessionId !== null && observed !== nativeSessionId) {
              throw new ProtocolError("Codex resumed a different native session");
            }
            if (nativeSessionId === null) {
              nativeSessionId = observed;
              const sessionAck = control.waitSessionAck(observed);
              write({ type: "session", session_id: observed });
              await sessionAck;
            }
          }
          if (event?.type === "item.completed" && event.item?.type === "agent_message") {
            finalResponse = event.item.text ?? "";
          }
          if (event?.type === "turn.completed") {
            usage = event.usage ?? null;
            turnCompleted = true;
          }
          // SDK item.started/updated/completed each carry the current item snapshot.
          const message = { type: "event", event: bounded(event, MAX_EVENT_BYTES) };
          streamBytes += bytes(JSON.stringify(message));
          if (streamBytes > MAX_STREAM_BYTES) throw new ProtocolError("sidecar output stream exceeded the size limit");
          write(message);
          if (event?.type === "turn.failed") throw new Error(event.error?.message || "Codex turn failed");
        }
        if (!turnCompleted) throw new Error("Codex stream ended without turn.completed");
        if (nativeSessionId === null) throw new Error("Codex stream ended without a native session ID");
        if (bytes(finalResponse) > MAX_RESULT_BYTES) throw new ProtocolError("Codex final response exceeded the size limit");
      })(), interrupted]);
    } catch (error) {
      failure = error;
    }
    controller.abort();
    const exited = await managed.stop();
    process.removeListener("SIGTERM", onSignal);
    process.removeListener("SIGINT", onSignal);
    managed.restore();
    closeInput();
    if (!exited) throw new ProtocolError("Codex cancellation could not confirm managed process exit");
    if (signalCancelled || control.cancelled) {
      write({ type: "error", session_id: nativeSessionId, confirmation: "managed_cli_exited", message: "Codex CLI exited; detached tool process exit is unconfirmed", uncertain: true });
      process.exitCode = 1;
      return;
    }
    if (control.failure) throw control.failure;
    if (failure) throw failure;
    write({ type: "result", session_id: nativeSessionId, final_response: redactString(finalResponse, secrets), usage: bounded(redact(usage, secrets), MAX_EVENT_BYTES) });
    void control.task;
  } finally { tools.close(); }
}

async function main() {
  const reader = frameReader(process.stdin);
  let outputSecrets = [];
  try {
    const input = validateInit(await readFrame(reader));
    outputSecrets = [process.env.OPENAI_API_KEY].filter((value) => typeof value === "string");
    await run(input, reader, outputSecrets);
  } catch (error) {
    const message = error instanceof Error ? error.message : "Codex SDK sidecar failed";
    try { write({ type: "error", message: redactString(message, outputSecrets), uncertain: true }); }
    catch { process.stderr.write(`${redactString(message, outputSecrets)}\n`); }
    closeInput();
    process.exitCode = 1;
  }
}

if (process.argv[2] === "tool") {
  try { await toolCommand(process.argv.slice(3)); }
  catch (error) { process.stderr.write(`${error instanceof ProtocolError ? error.message : "Node tool rejected or unavailable; preserve call_ref and query call_lookup before retrying"}\n`); process.exitCode = 1; }
} else {
  await main();
}
