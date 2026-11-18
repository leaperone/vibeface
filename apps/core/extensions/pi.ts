import { runHook } from "./transport.mts";

// Only a settled, user-driven TUI run can receive a follow-up.
const HOOK = __VIBEFACE_HOOK__;

interface PiMessage {
  role: string;
  content?: unknown;
  stopReason?: string;
  errorMessage?: string;
}

interface PiContext {
  mode: string;
  cwd: string;
  sessionManager: {
    getSessionId(): string;
    getLeafId(): string | null;
    getBranch(): readonly { type: string; message?: PiMessage }[];
  };
}

interface Pi {
  on(event: "agent_settled", handler: (event: unknown, ctx: PiContext) => void): void;
  sendUserMessage(text: string, options: { deliverAs: "followUp" }): void;
}

export default function (pi: Pi) {
  if (process.env.VIBEFACE_PI_RUN) return;
  // Pi waits for its handlers. Start the observation and return immediately.
  pi.on("agent_settled", (_event, ctx) => {
    try {
      if (ctx.mode === "tui") report(pi, ctx);
    } catch {}
  });
}

function report(pi: Pi, ctx: PiContext) {
  const session = ctx.sessionManager.getSessionId();
  const turn = ctx.sessionManager.getLeafId();
  const message = ctx.sessionManager.getBranch()
    .findLast((entry) => entry.type === "message")?.message;
  if (message?.role !== "assistant") return;
  const blocks: unknown[] = Array.isArray(message.content) ? message.content : [];
  const text = blocks.filter(isTextBlock).map((block) => block.text).join("\n\n");
  runHook(HOOK, {
    session_id: session,
    turn_id: turn,
    cwd: ctx.cwd,
    stop_reason: message.stopReason,
    error: message.errorMessage,
    last_message: text.slice(0, 8000),
  }, (answer) => {
    if (!answer || typeof answer !== "object" || !("text" in answer)) return;
    const next = answer.text;
    if (typeof next !== "string" || !next.trim()) return;
    if (ctx.sessionManager.getSessionId() !== session || ctx.sessionManager.getLeafId() !== turn) return;
    pi.sendUserMessage(next, { deliverAs: "followUp" });
  });
}

function isTextBlock(block: unknown): block is { type: "text"; text: string } {
  return typeof block === "object" && block !== null
    && "type" in block && block.type === "text"
    && "text" in block && typeof block.text === "string";
}
