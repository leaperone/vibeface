import { runHook } from "./transport.mts";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const HOOK = __VIBEFACE_HOOK__;
const ENV_DIR = process.env.VIBEFACE_OPENCODE_ENV;

// The SDK owns this client, including its transport, credentials and target.
interface Message {
  info: {
    id: string;
    role: string;
    error?: { name: string; data?: { message?: string } };
  };
  parts?: { type: string; synthetic?: boolean; text?: string }[];
}

interface Client {
  session: {
    get(input: { path: { id: string } }): PromiseLike<{ data?: { parentID?: string } }>;
    messages(input: {
      path: { id: string };
      query: { limit: number };
    }): PromiseLike<{ data?: Message[] }>;
  };
  _client: { post(input: { url: string; body?: object }): unknown };
}

interface ShellInput {
  sessionID?: string;
}
interface ShellOutput {
  env: Record<string, string>;
}

export default {
  id: HOOK ? "vibeface" : "vibeface-serve",
  server: async ({ client, directory }: { client: Client; directory: string }) => {
    if (ENV_DIR) {
      if (HOOK) return {};
      return {
        "shell.env": async (input: ShellInput, output: ShellOutput) => shellEnv(ENV_DIR, input, output),
      };
    }
    if (!HOOK) return {};
    return {
      // Opencode waits for handlers. Only start the observation here.
      event: async ({ event }: { event: unknown }) => {
        try {
          watch(client, directory, event);
        } catch {}
      },
    };
  },
};

function mergeEnv(file: string, env: ShellOutput["env"]) {
  let values: unknown;
  try {
    values = JSON.parse(readFileSync(file, "utf8"));
  } catch {
    return;
  }
  if (!values || typeof values !== "object" || Array.isArray(values)) return;
  for (const [key, value] of Object.entries(values)) {
    if (typeof value === "string") env[key] = value;
  }
}

function shellEnv(dir: string, input: ShellInput, output: ShellOutput) {
  mergeEnv(join(dir, "default.json"), output.env);
  const id = input?.sessionID;
  if (typeof id === "string" && /^ses_[A-Za-z0-9]+$/.test(id)) {
    mergeEnv(join(dir, `${id}.json`), output.env);
  }
}

function watch(client: Client, directory: string, event: unknown) {
  if (!event || typeof event !== "object" || !("type" in event) || typeof event.type !== "string") return;
  const asked = "properties" in event ? event.properties : undefined;
  if (!asked || typeof asked !== "object" || !("sessionID" in asked) || typeof asked.sessionID !== "string") return;
  const base = { event: event.type, session_id: asked.sessionID, cwd: directory };
  if (event.type === "session.idle") {
    settled(client, base).catch(() => {});
    return;
  }
  if (!("id" in asked) || typeof asked.id !== "string") return;
  const request = encodeURIComponent(asked.id);
  if (event.type === "permission.asked") {
    const permission = "permission" in asked ? asked.permission : undefined;
    runHook(HOOK, { ...base, request_id: asked.id, permission }, (answer) => {
      if (!answer || typeof answer !== "object" || !("reply" in answer)) return;
      const reply = answer.reply;
      if (reply !== "once" && reply !== "always" && reply !== "reject") return;
      const message = "message" in answer ? answer.message : undefined;
      if (message !== undefined && typeof message !== "string") return;
      const body = { reply, message };
      return client._client.post({ url: `/permission/${request}/reply`, body });
    });
  } else if (event.type === "question.asked") {
    const questions = "questions" in asked ? asked.questions : undefined;
    runHook(HOOK, { ...base, request_id: asked.id, questions }, (answer) => {
      if (!answer || typeof answer !== "object") return;
      if ("reject" in answer && answer.reject === true) {
        return client._client.post({ url: `/question/${request}/reject`, body: undefined });
      }
      if (!("answers" in answer) || !isAnswerMatrix(answer.answers)) return;
      return client._client.post({ url: `/question/${request}/reply`, body: { answers: answer.answers } });
    });
  }
}

function isAnswerMatrix(value: unknown): value is string[][] {
  return Array.isArray(value) && value.every((row: unknown) =>
    Array.isArray(row) && row.every((cell: unknown) => typeof cell === "string")
  );
}

async function settled(client: Client, base: { event: string; session_id: string; cwd: string }) {
  const path = { id: base.session_id };
  const session = (await client.session.get({ path })).data;
  if (!session || session.parentID) return;
  const messages = (await client.session.messages({ path, query: { limit: 10 } })).data ?? [];
  const reply = messages.findLast((message) => message.info?.role === "assistant");
  if (!reply) return;
  const error = reply.info.error;
  const status = !error
    ? "completed"
    : error.name === "MessageAbortedError" ? "interrupted" : "failed";
  const text = (reply.parts ?? []).filter((part) => part.type === "text" && !part.synthetic)
    .map((part) => part.text).join("\n\n");
  runHook(HOOK, {
    ...base,
    turn_id: reply.info.id,
    status,
    error: error && (error.data?.message ?? error.name),
    last_message: text.slice(0, 8000),
  });
}
