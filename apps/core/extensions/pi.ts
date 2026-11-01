// vibeface: `vibeface hooks install` writes this file and `vibeface hooks uninstall` removes it.
// @ts-nocheck
// When a run of a pi session the user drives settles, this hands it to the Node with
// `vibeface hook pi`. The Node may answer with the session's next user message; an answer
// that comes after the user went on is dropped. Runs the Node starts set VIBEFACE_PI_RUN.
import { spawn } from "node:child_process";

const HOOK = __VIBEFACE_HOOK__;

export default function (pi) {
  if (process.env.VIBEFACE_PI_RUN) return;
  // pi waits for its handlers; this one only starts the hook.
  pi.on("agent_settled", (_event, ctx) => {
    try {
      if (ctx.mode === "tui") report(pi, ctx);
    } catch {}
  });
}

function report(pi, ctx) {
  const session = ctx.sessionManager.getSessionId();
  const turn = ctx.sessionManager.getLeafId();
  const message = ctx.sessionManager
    .getBranch()
    .findLast((entry) => entry.type === "message")?.message;
  if (message?.role !== "assistant") return;
  const text = (Array.isArray(message.content) ? message.content : [])
    .filter((block) => block.type === "text")
    .map((block) => block.text)
    .join("\n\n");
  const hook = spawn(HOOK[0], HOOK.slice(1), { stdio: ["pipe", "pipe", "ignore"] });
  let answer = "";
  hook.stdout.setEncoding("utf8");
  hook.stdout.on("data", (chunk) => (answer += chunk));
  hook.on("error", () => {});
  hook.stdin.on("error", () => {});
  hook.on("close", () => {
    try {
      const next = JSON.parse(answer).text;
      if (typeof next !== "string" || !next.trim()) return;
      if (ctx.sessionManager.getSessionId() !== session) return;
      if (ctx.sessionManager.getLeafId() !== turn) return;
      pi.sendUserMessage(next, { deliverAs: "followUp" });
    } catch {}
  });
  hook.stdin.end(
    JSON.stringify({
      session_id: session,
      turn_id: turn,
      cwd: ctx.cwd,
      stop_reason: message.stopReason,
      error: message.errorMessage,
      last_message: text.slice(0, 8000),
    }),
  );
}
