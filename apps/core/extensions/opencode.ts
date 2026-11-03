// vibeface: `vibeface hooks install` writes this file and `vibeface hooks uninstall` removes it.
// @ts-nocheck
// In opencode sessions the user drives, this hands each settled run and each permission or
// question a session asks to the Node with `vibeface hook opencode`, without holding opencode
// up. The Node may answer a permission or question, and this replies with it. The Node's own
// serve loads a copy whose HOOK is null: there it only gives each session's tool shells the tool
// environment of its turn, which the Node keeps in VIBEFACE_OPENCODE_ENV.
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const HOOK = __VIBEFACE_HOOK__;
const ENV_DIR = process.env.VIBEFACE_OPENCODE_ENV;

export default {
  id: HOOK ? "vibeface" : "vibeface-serve",
  server: async ({ client, directory }) => {
    if (ENV_DIR) return HOOK ? {} : { "shell.env": shellEnv };
    if (!HOOK) return {};
    return {
      // opencode waits for its event handlers; this one only starts the hook.
      event: async ({ event }) => {
        try {
          watch(client, directory, event);
        } catch {}
      },
    };
  },
};

function read(file) {
  try {
    return JSON.parse(readFileSync(file, "utf8"));
  } catch {
    return {};
  }
}

// The serve's own settings and anything that looks like a key are blanked first.
async function shellEnv(input, output) {
  Object.assign(output.env, read(join(ENV_DIR, "default.json")));
  const id = input?.sessionID;
  if (typeof id === "string" && /^ses_[A-Za-z0-9]+$/.test(id)) {
    Object.assign(output.env, read(join(ENV_DIR, `${id}.json`)));
  }
}

function watch(client, directory, event) {
  const asked = event.properties ?? {};
  const base = { event: event.type, session_id: asked.sessionID, cwd: directory };
  const request = encodeURIComponent(asked.id);
  if (event.type === "session.idle") {
    settled(client, base).catch(() => {});
  } else if (event.type === "permission.asked") {
    const payload = { ...base, request_id: asked.id, permission: asked.permission };
    hook(payload, (answer) => {
      if (typeof answer.reply !== "string") return;
      const body = { reply: answer.reply, message: answer.message };
      return post(client, `/permission/${request}/reply`, body);
    });
  } else if (event.type === "question.asked") {
    const payload = { ...base, request_id: asked.id, questions: asked.questions };
    hook(payload, (answer) => {
      if (answer.reject === true) return post(client, `/question/${request}/reject`);
      if (!Array.isArray(answer.answers)) return;
      return post(client, `/question/${request}/reply`, { answers: answer.answers });
    });
  }
}

// A run of a session settled: its last reply and how it ended. Sub-agents' sessions are left out.
async function settled(client, base) {
  const path = { id: base.session_id };
  const session = (await client.session.get({ path })).data;
  if (!session || session.parentID) return;
  const messages = (await client.session.messages({ path, query: { limit: 10 } })).data ?? [];
  const reply = messages.findLast((message) => message.info?.role === "assistant");
  if (!reply) return;
  const error = reply.info.error;
  const status = !error
    ? "completed"
    : error.name === "MessageAbortedError"
      ? "interrupted"
      : "failed";
  const text = (reply.parts ?? [])
    .filter((part) => part.type === "text" && !part.synthetic)
    .map((part) => part.text)
    .join("\n\n");
  hook({
    ...base,
    turn_id: reply.info.id,
    status,
    error: error && (error.data?.message ?? error.name),
    last_message: text.slice(0, 8000),
  });
}

// `vibeface hook opencode` with the payload; `answered` gets the Node's answer when it gave one.
function hook(payload, answered) {
  const child = spawn(HOOK[0], HOOK.slice(1), { stdio: ["pipe", "pipe", "ignore"] });
  let answer = "";
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (chunk) => (answer += chunk));
  child.on("error", () => {});
  child.stdin.on("error", () => {});
  child.on("close", () => {
    try {
      if (answered && answer.trim()) Promise.resolve(answered(JSON.parse(answer))).catch(() => {});
    } catch {}
  });
  child.stdin.end(JSON.stringify(payload));
}

// opencode's own client, which reaches this opencode however it serves.
function post(client, url, body) {
  return client._client.post({ url, body });
}
