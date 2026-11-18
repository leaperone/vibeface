import { spawn } from "node:child_process";

// The CLI accepts a 1 MiB Node response and prints its compact JSON plus a newline.
// Leave room for re-encoding, and for local git probes before the 5s POST and 40s wait.
const MAX_STDOUT_BYTES = 2 * 1024 * 1024;
const HOOK_LIFETIME_MS = 90_000;

export function runHook(
  command: readonly string[] | null,
  payload: object,
  answered?: (value: unknown) => unknown,
) {
  if (!command?.[0]) return;
  const input = JSON.stringify(payload);
  const child = spawn(command[0], command.slice(1), { stdio: ["pipe", "pipe", "ignore"] });
  const chunks: Buffer[] = [];
  let bytes = 0;
  let stopped = false;
  const timer = setTimeout(cancel, HOOK_LIFETIME_MS);
  timer.unref();

  function cancel() {
    if (stopped) return;
    stopped = true;
    clearTimeout(timer);
    chunks.length = 0;
    child.stdin.destroy();
    child.stdout.destroy();
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
  }

  child.on("error", cancel);
  child.stdin.on("error", cancel);
  child.stdout.on("error", cancel);
  child.stdout.on("data", (chunk: Buffer) => {
    if (stopped) return;
    bytes += chunk.length;
    if (bytes > MAX_STDOUT_BYTES) cancel();
    else if (answered) chunks.push(chunk);
  });
  child.once("close", (code) => {
    clearTimeout(timer);
    if (stopped || code !== 0 || !answered || bytes === 0) return;
    try {
      // Keep a leading BOM in the text so JSON.parse enforces the original JSON grammar.
      const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
      const value: unknown = JSON.parse(decoder.decode(Buffer.concat(chunks, bytes)));
      Promise.resolve(answered(value)).catch(() => {});
    } catch {}
  });
  child.stdin.end(input);
}
