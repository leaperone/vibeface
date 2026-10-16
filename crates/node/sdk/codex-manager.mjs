import { access, constants, stat } from "node:fs/promises";
import path from "node:path";
import { Codex } from "@openai/codex-sdk";

const MAX_FRAME_BYTES = 1_048_576;
const MAX_PROMPT_BYTES = 65_536;
const MAX_RESULT_BYTES = 65_536;
const MAX_EVENT_BYTES = 65_536;
const MAX_STREAM_BYTES = 16_777_216;
const MAX_ID_BYTES = 256;
const MAX_MODEL_BYTES = 256;
const MAX_SERVERS = 8;
const MAX_ARGS = 64;
const MAX_ENV = 64;
const MAX_ENV_VALUE_BYTES = 4_096;
const MAX_TOOLS = 128;
const ID_PATTERN = /^[^\u0000-\u001f\u007f\r\n]{1,256}$/;
const NAME_PATTERN = /^[A-Za-z0-9_-]{1,64}$/;
const ENV_KEY_PATTERN = /^[A-Za-z_][A-Za-z0-9_]{0,255}$/;
const RESERVED_TOOL_ENV = new Set([
  "CODEX_HOME",
  "CODEX_API_KEY",
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
  "mcp_servers",
  "tool_cli",
  "tool_env",
]);

const MCP_FIELDS = new Set(["command", "args", "env", "enabled_tools"]);

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

function validateArgs(args, name) {
  if (args === undefined) return [];
  if (!Array.isArray(args) || args.length > MAX_ARGS) {
    throw new ProtocolError(`${name}.args must contain at most ${MAX_ARGS} strings`);
  }
  return args.map((arg, index) => {
    if (typeof arg !== "string" || bytes(arg) > 4_096 || arg.includes("\0")) {
      throw new ProtocolError(`${name}.args[${index}] is invalid`);
    }
    return arg;
  });
}

function validateEnv(env, name, { rejectReserved = false } = {}) {
  if (env === undefined) return {};
  if (!isObject(env) || Object.keys(env).length > MAX_ENV) {
    throw new ProtocolError(`${name} must contain at most ${MAX_ENV} entries`);
  }
  const result = {};
  for (const [key, value] of Object.entries(env)) {
    if (!ENV_KEY_PATTERN.test(key) || (rejectReserved && RESERVED_TOOL_ENV.has(key))) {
      throw new ProtocolError(`${name} contains an invalid or reserved key`);
    }
    if (typeof value !== "string" || bytes(value) > MAX_ENV_VALUE_BYTES || value.includes("\0")) {
      throw new ProtocolError(`${name}.${key} is invalid`);
    }
    result[key] = value;
  }
  return result;
}

function validateEnabledTools(value, name) {
  if (value === undefined) return undefined;
  if (!Array.isArray(value) || value.length > MAX_TOOLS) {
    throw new ProtocolError(`${name}.enabled_tools must contain at most ${MAX_TOOLS} names`);
  }
  return value.map((tool, index) => {
    if (typeof tool !== "string" || bytes(tool) === 0 || bytes(tool) > 256 || tool.includes("\0")) {
      throw new ProtocolError(`${name}.enabled_tools[${index}] is invalid`);
    }
    return tool;
  });
}

function validateMcpServers(value) {
  if (value === undefined) return {};
  if (!isObject(value) || Object.keys(value).length > MAX_SERVERS) {
    throw new ProtocolError(`mcp_servers must contain at most ${MAX_SERVERS} servers`);
  }
  const result = {};
  for (const [name, raw] of Object.entries(value)) {
    if (!NAME_PATTERN.test(name) || !isObject(raw)) {
      throw new ProtocolError(`invalid MCP server ${name}`);
    }
    rejectUnknownFields(raw, MCP_FIELDS, `mcp_servers.${name}`);
    result[name] = {
      command: requireAbsolutePath(raw.command, `mcp_servers.${name}.command`),
      args: validateArgs(raw.args, `mcp_servers.${name}`),
      env: validateEnv(raw.env, `mcp_servers.${name}.env`),
      enabled_tools: validateEnabledTools(raw.enabled_tools, `mcp_servers.${name}`),
      required: true,
    };
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
  const sessionId = requireId(value.session_id);
  const model = value.model === undefined || value.model === null || value.model === ""
    ? undefined
    : requireString(value.model, "model", MAX_MODEL_BYTES);
  const toolCli = value.tool_cli === undefined || value.tool_cli === null ? undefined : value.tool_cli;
  if (toolCli !== undefined) requireAbsolutePath(toolCli, "tool_cli");
  return {
    prompt,
    sessionId,
    cwd: value.cwd,
    model,
    codexPath: value.codex_path,
    mcpServers: validateMcpServers(value.mcp_servers),
    toolCli,
    toolEnv: validateEnv(value.tool_env, "tool_env", { rejectReserved: true }),
  };
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

function sdkConfig(input) {
  const mcpServers = {};
  for (const [name, server] of Object.entries(input.mcpServers)) {
    mcpServers[name] = {
      command: server.command,
      args: server.args,
      env: server.env,
      required: server.required,
      ...(server.enabled_tools ? { enabled_tools: server.enabled_tools } : {}),
    };
  }
  return {
    approval_policy: "never",
    default_permissions: "vibeface_manager",
    web_search: "disabled",
    check_for_update_on_startup: false,
    analytics: { enabled: false },
    feedback: false,
    include_apps_instructions: false,
    include_collaboration_mode_instructions: false,
    include_environment_context: false,
    include_environment_context_time: false,
    include_permissions_instructions: false,
    mcp_servers: mcpServers,
    features: {
      shell_tool: true,
      unified_exec: false,
      unified_exec_tty: false,
      stable_environment_tools: false,
      view_image: false,
      sleep_tool: false,
      request_permissions_tool: false,
      web_search_request: false,
      web_search_cached: false,
      standalone_web_search: false,
      code_mode: false,
      code_mode_host: false,
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
    experimental_use_unified_exec_tool: false,
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
  for (const key of allowed) {
    if (process.env[key] !== undefined) env[key] = process.env[key];
  }
  for (const [key, value] of Object.entries(input.toolEnv)) env[key] = value;
  return env;
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
  if (line.done) throw new ProtocolError("sidecar input closed before the next frame");
  try {
    return JSON.parse(line.value);
  } catch {
    throw new ProtocolError("sidecar input was not valid JSON");
  }
}

async function run(input, reader, secrets) {
  const cwd = await validateCwd(input.cwd);
  const codexPath = await validateExecutable(input.codexPath, "codex_path");
  const toolCli = input.toolCli === undefined ? undefined : await validateExecutable(input.toolCli, "tool_cli");
  if (toolCli !== undefined) input.toolEnv.VIBEFACE_TOOL_CLI = toolCli;

  const codex = new Codex({
    codexPathOverride: codexPath,
    config: sdkConfig(input),
    configOverrides: [
      'permissions.vibeface_manager.filesystem={":root"="read"}',
      'permissions.vibeface_manager.network={enabled=true,allow_local_binding=true,mode="limited",domains={"127.0.0.1"="allow","localhost"="allow"}}',
    ],
    env: childEnvironment(input),
  });
  const thread = codex.resumeThread(input.sessionId, {
    model: input.model,
    threadSource: "vibeface-node-manager",
    workingDirectory: cwd,
    approvalPolicy: "never",
    webSearchMode: "disabled",
  });

  write({ type: "ready", session_id: input.sessionId, protocol: 1 });
  const ack = await readFrame(reader);
  if (!isObject(ack) || ack.type !== "ack" || ack.session_id !== input.sessionId || Object.keys(ack).some((key) => !["type", "session_id"].includes(key))) {
    throw new ProtocolError("expected an ACK for the confirmed session");
  }

  const streamed = await thread.runStreamed(input.prompt);
  let finalResponse = "";
  let usage = null;
  let turnCompleted = false;
  let streamBytes = 0;
  for await (const rawEvent of streamed.events) {
    const event = redact(rawEvent, secrets);
    if (event?.type === "thread.started" && event.thread_id !== input.sessionId) {
      throw new ProtocolError("Codex resumed a different native session");
    }
    if (event?.type === "item.completed" && event.item?.type === "agent_message") {
      finalResponse = event.item.text ?? "";
    }
    if (event?.type === "turn.completed") {
      usage = event.usage ?? null;
      turnCompleted = true;
    }
    const message = { type: "event", event: bounded(event, MAX_EVENT_BYTES) };
    streamBytes += bytes(JSON.stringify(message));
    if (streamBytes > MAX_STREAM_BYTES) throw new ProtocolError("sidecar output stream exceeded the size limit");
    write(message);
    if (event?.type === "turn.failed") throw new Error(event.error?.message || "Codex turn failed");
  }
  if (!turnCompleted) throw new Error("Codex stream ended without turn.completed");
  if (bytes(finalResponse) > MAX_RESULT_BYTES) throw new ProtocolError("Codex final response exceeded the size limit");
  write({ type: "result", session_id: input.sessionId, final_response: redactString(finalResponse, secrets), usage: bounded(redact(usage, secrets), MAX_EVENT_BYTES) });
}

const reader = frameReader(process.stdin);
let outputSecrets = [];
try {
  const input = validateInit(await readFrame(reader));
  outputSecrets = [
    ...Object.values(input.toolEnv),
    ...Object.values(input.mcpServers).flatMap((server) => Object.values(server.env)),
    process.env.CODEX_API_KEY,
    process.env.OPENAI_API_KEY,
  ].filter((value) => typeof value === "string");
  await run(input, reader, outputSecrets);
} catch (error) {
  const message = error instanceof Error ? error.message : "Codex SDK sidecar failed";
  try {
    write({ type: "error", message: redactString(message, outputSecrets), uncertain: true });
  } catch {
    process.stderr.write(`${redactString(message, outputSecrets)}\n`);
  }
  process.exitCode = 1;
}
