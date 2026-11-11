/** Synthetic Node main-loop probe, not browser frame-rate evidence. Run with node --experimental-strip-types. */
import assert from "node:assert/strict"
import { createHash } from "node:crypto"
import { MarkdownScheduler, type MarkdownResult } from "./markdown-scheduler.ts"
import { renderMarkdown } from "./markdown.ts"
import { createNodeMarkdownWorker } from "./markdown-worker-test-adapter.ts"

const delay = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms))
const block = (index: number) => `\n\n## Section ${index}\n\n` + "A **streamed** paragraph with [a safe link](https://example.com) and `inline code`.\n".repeat(12)
  + "\n| key | value |\n|---|---|\n|a|b|\n\n```ts\n" + Array.from({ length: 20 }, (_, line) => `const v${line} = "<escaped>"`).join("\n") + "\n```\n"
const snapshots: string[] = []
let text = ""
for (let index = 0; index < 96; index++) {
  text += block(index)
  snapshots.push(text)
}

async function measure(run: () => Promise<string>) {
  const gaps: number[] = []
  let previous = performance.now()
  const heartbeat = setInterval(() => {
    const now = performance.now()
    gaps.push(Math.max(0, now - previous - 2))
    previous = now
  }, 2)
  const started = performance.now()
  try {
    const html = await run()
    await delay(5)
    const elapsedMs = performance.now() - started
    gaps.sort((a, b) => a - b)
    return {
      html,
      measurement: {
        elapsedMs, heartbeatSamples: gaps.length,
        maxHeartbeatDelayMs: gaps.at(-1) ?? 0,
        p95HeartbeatDelayMs: gaps[Math.floor(gaps.length * 0.95)] ?? 0,
      },
    }
  } finally { clearInterval(heartbeat) }
}

// Warm the actual parser before comparing execution paths.
renderMarkdown(snapshots[0] ?? "")
const baseline = await measure(async () => {
  let html = ""
  for (const snapshot of snapshots) {
    await delay(2)
    html = renderMarkdown(snapshot)
  }
  return html
})

let parses = 0
let deliver: (result: MarkdownResult) => void = () => {}
const scheduler = new MarkdownScheduler(() => {
  const worker = createNodeMarkdownWorker()
  const post = worker.postMessage
  worker.postMessage = (request) => { parses++; post(request) }
  return worker
})
const message = scheduler.connect((result) => deliver(result))
try {
  await new Promise<void>((resolve, reject) => {
    deliver = (result) => result.kind === "rendered" ? resolve() : reject(new Error(result.kind))
    message.update("warm up", false)
  })
  parses = 0
  const updated = await measure(async () => {
    let final: (html: string) => void = () => {}
    let fail: (error: Error) => void = () => {}
    const finished = new Promise<string>((resolve, reject) => { final = resolve; fail = reject })
    deliver = (result) => {
      if (result.kind !== "rendered") return fail(new Error(result.kind))
      if (result.text === text) final(result.html)
    }
    for (const snapshot of snapshots) {
      await delay(2)
      message.update(snapshot, true)
    }
    message.update(text, false)
    return finished
  })
  assert.equal(updated.html, baseline.html)
  assert.equal(updated.html, renderMarkdown(text))
  process.stdout.write(JSON.stringify({
    scope: "Synthetic Node worker_threads main-loop responsiveness; no browser FPS claim",
    node: process.version, updates: snapshots.length, finalCharacters: text.length,
    outputMatchesBaselineAndParser: true,
    outputSha256: createHash("sha256").update(updated.html).digest("hex"),
    baseline: { parses: snapshots.length, ...baseline.measurement },
    worker: { parses, ...updated.measurement },
  }, null, 2) + "\n")
} finally { message.dispose() }
