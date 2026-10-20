import childProcess from "node:child_process";
import { access, constants, stat } from "node:fs/promises";
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
const MAX_ENV = 64;
const MAX_ENV_VALUE_BYTES = 4_096;
const ID_PATTERN = /^[^\u0000-\u001f\u007f\r\n]{1,256}$/;
const ENV_KEY_PATTERN = /^[A-Za-z_][A-Za-z0-9_]{0,255}$/;
const RESERVED_TOOL_ENV = new Set([
  "CODEX_HOME",
  "CODEX_API_KEY",
  "CURSOR_API_KEY",
  "HOME",
  "PATH",
  "OPENAI_API_KEY",
  "OPENAI_BASE_URL",
]);

const INIT_FIELDS = new Set([
  "type",
  "prompt",
  "session_id",
  "cwd",
  "model",
  "codex_path",
  "tool_cli",
  "tool_env",
  "custom_tools",
  "read_only",
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

function validateEnv(env, name) {
  if (env === undefined) return {};
  if (!isObject(env) || Object.keys(env).length > MAX_ENV) {
    throw new ProtocolError(`${name} must contain at most ${MAX_ENV} entries`);
  }
  const result = {};
  for (const [key, value] of Object.entries(env)) {
    if (!ENV_KEY_PATTERN.test(key) || RESERVED_TOOL_ENV.has(key)) {
      throw new ProtocolError(`${name} contains an invalid or reserved key`);
    }
    if (typeof value !== "string" || bytes(value) > MAX_ENV_VALUE_BYTES || value.includes("\0")) {
      throw new ProtocolError(`${name}.${key} is invalid`);
    }
    result[key] = value;
  }
  return result;
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
  const toolCli = value.tool_cli === undefined || value.tool_cli === null ? undefined : value.tool_cli;
  if (toolCli !== undefined) requireAbsolutePath(toolCli, "tool_cli");
  if (value.read_only !== undefined && typeof value.read_only !== "boolean") throw new ProtocolError("read_only must be a boolean");
  const toolEnv = validateEnv(value.tool_env, "tool_env");
  const readOnly = value.read_only ?? false;
  return {
    prompt,
    sessionId,
    cwd: value.cwd,
    model,
    codexPath: value.codex_path,
    toolCli,
    toolEnv,
    customTools: validateTools(value.custom_tools),
    readOnly,
  };
}

function validateTools(value) {
  if (value === undefined) return {};
  if (!isObject(value) || Object.keys(value).length > 32 || bytes(JSON.stringify(value)) > MAX_EVENT_BYTES) throw new ProtocolError("custom_tools must be a bounded tool catalog");
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

function sdkConfig(input) {
  return {
    approval_policy: "never",
    default_permissions: "vibeface_manager",
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
      include_only: ["HOME", "PATH", "TMPDIR", "LANG", "LC_ALL", ...Object.keys(input.toolEnv)],
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

function childEnvironment(input) {
  const allowed = [
    "CODEX_HOME",
    "CODEX_API_KEY",
    "HOME",
    "PATH",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
  ];
  const env = {};
  const providerKeys = JSON.parse(process.env.VIBEFACE_CODEX_PROVIDER_ENV_KEYS || "[]");
  if (!Array.isArray(providerKeys) || providerKeys.length > MAX_ENV || providerKeys.some((key) => typeof key !== "string" || !ENV_KEY_PATTERN.test(key))) throw new ProtocolError("invalid provider environment key list");
  allowed.push(...providerKeys);
  for (const key of allowed) {
    if (process.env[key] !== undefined) env[key] = process.env[key];
  }
  for (const [key, value] of Object.entries(input.toolEnv)) env[key] = value;
  return env;
}

function managerPrompt(prompt, toolCli, tools, readOnly) {
  if (toolCli === undefined) return prompt;
  const executable = `'${toolCli.replaceAll("'", "'\"'\"'")}'`;
  const catalog = Object.entries(tools).map(([name, spec]) => `${name}\n${spec.description || ""}\nJSON schema: ${JSON.stringify(spec.inputSchema)}`).join("\n\n");
  const host = readOnly
    ? "This invocation is read-only. Read existing status and files; do not edit files, start work, interrupt work, or answer native requests."
    : "Your shell and file tools can read this host but cannot change it. To change files on this host, call dispatch_work with a prompt and a cwd inside an allowed root. The Node starts one native Codex turn, pins approval to the user and the sandbox to that cwd, and you cannot answer its approval requests. Your turn ends after dispatch; when the work finishes, the Node starts a new turn with its result.";
  const result = `Node tools are available through your command-execution tool. Run:\n${executable} tool TOOL_NAME --json 'JSON_OBJECT'\nFor dispatch_work, update_profile, save_skill, remove_skill, save_routine and remove_routine, add --call-ref 'UNIQUE_INTENT_REFERENCE'. Choose the reference once before the first request and preserve it across CLI processes. Reuse that reference with identical arguments after a partial response. On a timeout, query call_lookup with {"call_ref":"THE_SAME_REFERENCE"}; never replace an uncertain call with a new reference. Different authorized intentions need different references.\nThe CLI uses its scoped loopback connection and prints JSON. Do not read or print credential variables. Report failed calls honestly. Conversation and network scope come from the CLI. ${host}\n\nAvailable tools for this invocation:\n${catalog}\n\n${prompt}`;
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

function startControlPump(reader, controller, sessionId) {
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
  const alive = (pid) => {
    try {
      process.kill(pid, 0);
      return true;
    } catch (error) {
      return error.code !== "ESRCH";
    }
  };
  // Every process that was in the CLI tree while it was stopped, with its group.
  const tracked = new Map();
  const track = () => {
    let table;
    try {
      table = childProcess.execFileSync("/bin/ps", ["-A", "-o", "pid=,ppid=,pgid="], { encoding: "utf8", timeout: 1_000 });
    } catch {
      return false;
    }
    const rows = table.trim().split("\n").map((line) => line.trim().split(/\s+/).map(Number));
    const roots = new Set([child.pid, ...tracked.keys()]);
    for (let grew = true; grew;) {
      grew = false;
      for (const [pid, ppid, pgid] of rows) {
        if (!roots.has(ppid) || roots.has(pid)) continue;
        roots.add(pid);
        tracked.set(pid, pgid);
        grew = true;
      }
    }
    return true;
  };
  const signalTree = (name) => {
    for (const pgid of new Set([child.pid, ...tracked.values()])) {
      try { process.kill(-pgid, name); } catch {}
    }
  };
  const treeExited = () => !alive(-child.pid)
    && [...tracked].every(([pid, pgid]) => !alive(pid) && !alive(-pgid));
  const stop = () => stopping ??= (async () => {
    if (!child) return true;
    if (child.pid && process.platform !== "win32") {
      // Tools may leave the CLI group, so each descendant group seen while the tree is alive is killed and checked.
      // Processes that already left the tree (re-parented away) before stop are invisible here, as on normal completion.
      let observed = track();
      signalTree("SIGTERM");
      const started = performance.now();
      while (performance.now() - started < 1_500) {
        observed &&= track();
        if (observed && closed && treeExited()) return true;
        if (performance.now() - started >= 300) signalTree("SIGKILL");
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
  const toolCli = input.toolCli === undefined ? undefined : await validateExecutable(input.toolCli, "tool_cli");
  const prompt = managerPrompt(input.prompt, toolCli, input.customTools, input.readOnly);
  const codex = new Codex({
    codexPathOverride: codexPath,
    config: sdkConfig(input),
    configOverrides: [
      'permissions.vibeface_manager.filesystem={":root"="read"}',
      'permissions.vibeface_manager.network={enabled=true,allow_local_binding=true,mode="limited",domains={"127.0.0.1"="allow","localhost"="allow"}}',
    ],
    env: childEnvironment(input),
  });
  const threadOptions = {
    model: input.model,
    threadSource: "vibeface-manager",
    workingDirectory: cwd,
    skipGitRepoCheck: true,
    approvalPolicy: "never",
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
  const control = startControlPump(reader, controller, () => nativeSessionId);
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
    if (process.platform === "win32") {
      write({ type: "error", session_id: nativeSessionId, confirmation: "managed_cli_exited", message: "Codex CLI exited; tool process exit is unconfirmed on Windows", uncertain: true });
      process.exitCode = 1;
      return;
    }
    write({ type: "cancelled", session_id: nativeSessionId, confirmation: "managed_processes_exited" });
    return;
  }
  if (control.failure) throw control.failure;
  if (failure) throw failure;
  write({ type: "result", session_id: nativeSessionId, final_response: redactString(finalResponse, secrets), usage: bounded(redact(usage, secrets), MAX_EVENT_BYTES) });
  void control.task;
}

const reader = frameReader(process.stdin);
let outputSecrets = [];
try {
  const input = validateInit(await readFrame(reader));
  outputSecrets = [
    ...Object.values(input.toolEnv),
    process.env.CODEX_API_KEY,
    process.env.OPENAI_API_KEY,
    ...Object.entries(process.env).filter(([key]) => /(?:_KEY|_TOKEN|_SECRET|_PASSWORD)$/.test(key)).map(([, value]) => value),
  ].filter((value) => typeof value === "string");
  await run(input, reader, outputSecrets);
} catch (error) {
  const message = error instanceof Error ? error.message : "Codex SDK sidecar failed";
  try {
    write({ type: "error", message: redactString(message, outputSecrets), uncertain: true });
  } catch {
    process.stderr.write(`${redactString(message, outputSecrets)}\n`);
  }
  closeInput();
  process.exitCode = 1;
}
