import assert from "node:assert/strict"
import test from "node:test"
import { MarkdownScheduler, MAX_PENDING_MARKDOWN, type MarkdownResult, type MarkdownWorker } from "./markdown-scheduler.ts"
import { renderMarkdown } from "./markdown.ts"
import type { MarkdownRequest } from "./markdown-protocol.ts"
import { createNodeMarkdownWorker } from "./markdown-worker-test-adapter.ts"

function controlled() {
  let now = 0
  let terminated = 0
  const sent: MarkdownRequest[] = []
  const timers = new Map<ReturnType<typeof setTimeout>, { due: number; run: () => void }>()
  const worker: MarkdownWorker = {
    postMessage: (request) => sent.push(request), terminate: () => { terminated++ },
    onmessage: null, onerror: null, onmessageerror: null,
  }
  const scheduler = new MarkdownScheduler(() => worker, {
    now: () => now,
    setTimeout: (run, delay) => {
      const timer = setTimeout(() => {}, 100_000)
      clearTimeout(timer)
      timers.set(timer, { due: now + delay, run })
      return timer
    },
    clearTimeout: (timer) => { clearTimeout(timer); timers.delete(timer) },
  })
  return {
    scheduler, sent, worker,
    terminated: () => terminated,
    advance(ms: number) {
      now += ms
      for (const [timer, task] of [...timers]) if (task.due <= now) {
        clearTimeout(timer)
        timers.delete(timer)
        task.run()
      }
    },
    finish(index: number, html: string) {
      const request = sent[index]
      assert.ok(request)
      worker.onmessage?.({ data: { kind: "rendered", id: request.id, html } })
    },
  }
}

test("stream updates coalesce, stale results cannot overwrite the latest, and terminal text flushes", () => {
  const harness = controlled()
  const results: MarkdownResult[] = []
  const message = harness.scheduler.connect((result) => results.push(result))
  message.update("one", true)
  harness.advance(30)
  message.update("one two", true)
  harness.advance(30)
  assert.deepEqual(harness.sent.map((request) => request.text), ["one two"])
  message.update("one two three", true)
  message.update("one two three **done**", false)
  harness.finish(0, "stale")
  assert.deepEqual(harness.sent.map((request) => request.text), ["one two", "one two three **done**"])
  harness.finish(1, "<p>one two three <strong>done</strong></p>\n")
  assert.deepEqual(results, [{ kind: "rendered", text: "one two three **done**", html: "<p>one two three <strong>done</strong></p>\n" }])
  message.dispose()
  assert.equal(harness.terminated(), 1)
})

test("a completed message takes priority over queued streaming snapshots", () => {
  const harness = controlled()
  const results: MarkdownResult[] = []
  const first = harness.scheduler.connect(() => {})
  const stream = harness.scheduler.connect(() => {})
  const final = harness.scheduler.connect((result) => results.push(result))
  first.update("working", false)
  stream.update("still growing", true)
  harness.advance(100)
  final.update("finished", false)
  harness.finish(0, "<p>working</p>\n")
  assert.equal(harness.sent[1]?.text, "finished")
  harness.finish(1, "<p>finished</p>\n")
  assert.deepEqual(results, [{ kind: "rendered", text: "finished", html: "<p>finished</p>\n" }])
  first.dispose(); stream.dispose(); final.dispose()
})

test("unmount cancels queued and in-flight callbacks, and releasing all messages terminates the worker", () => {
  const harness = controlled()
  const results: MarkdownResult[] = []
  const first = harness.scheduler.connect((result) => results.push(result))
  const second = harness.scheduler.connect((result) => results.push(result))
  first.update("unmounted", false)
  second.update("also unmounted", true)
  first.dispose()
  harness.finish(0, "obsolete")
  second.update("visible", false)
  harness.finish(1, "<p>visible</p>\n")
  assert.deepEqual(results, [{ kind: "rendered", text: "visible", html: "<p>visible</p>\n" }])
  second.update("cancel queued", true)
  second.dispose()
  harness.advance(100)
  assert.equal(harness.sent.length, 2)
  assert.equal(harness.terminated(), 1)
})

test("queue pressure is bounded and a rejected complete message can retry", () => {
  const harness = controlled()
  const results: MarkdownResult[] = []
  const blocker = harness.scheduler.connect(() => {})
  blocker.update("running", false)
  const queued = Array.from({ length: MAX_PENDING_MARKDOWN }, (_, index) => {
    const message = harness.scheduler.connect(() => {})
    message.update(String(index), true)
    return message
  })
  const overflow = harness.scheduler.connect((result) => results.push(result))
  overflow.update("full final text", false)
  assert.deepEqual(results, [{ kind: "busy" }])
  queued[0]?.dispose()
  overflow.update("full final text", false)
  harness.finish(0, "discard")
  assert.equal(harness.sent[1]?.text, "full final text")
  harness.finish(1, "<p>full final text</p>\n")
  assert.deepEqual(results[1], { kind: "rendered", text: "full final text", html: "<p>full final text</p>\n" })
  blocker.dispose(); overflow.dispose()
  for (const message of queued) message.dispose()
})

test("unsupported or failed workers return a readable fallback signal without synchronous parsing", () => {
  const results: MarkdownResult[] = []
  const unsupported = new MarkdownScheduler(() => undefined).connect((result) => results.push(result))
  unsupported.update("<script>safe as React text</script>", false)
  assert.deepEqual(results, [{ kind: "unavailable" }])
  unsupported.dispose()
  const harness = controlled()
  const connected = harness.scheduler.connect((result) => results.push(result))
  connected.update("working", false)
  harness.worker.onerror?.()
  connected.update("final", false)
  assert.deepEqual(results, [{ kind: "unavailable" }, { kind: "unavailable" }, { kind: "unavailable" }])
  connected.dispose()
})

test("actual worker matches markdown-it for final code, folds, tables, links and hostile HTML", { timeout: 10_000 }, async () => {
  const scheduler = new MarkdownScheduler(createNodeMarkdownWorker)
  const text = '# Heading\n\n<script>alert(1)</script>\n\n[a](javascript:alert(1)) [safe](https://example.com) ![tracker](https://tracker.example/a.png)\n\n| a |\n|---|\n| 1 |\n\n```ts\n' + Array.from({ length: 20 }, (_, i) => `const line${i} = "<tag>"`).join("\n") + '\n```\n'
  let dispose = () => {}
  try {
    const result = await new Promise<MarkdownResult>((resolve) => {
      const message = scheduler.connect(resolve)
      dispose = message.dispose
      message.update(text.slice(0, 40), true)
      message.update(text, false)
    })
    assert.equal(result.kind, "rendered")
    if (result.kind !== "rendered") throw new Error("Worker failed")
    assert.equal(result.text, text)
    assert.equal(result.html, renderMarkdown(text))
    assert.ok(result.html.includes("<h1>Heading</h1>"))
    assert.ok(result.html.includes('data-copy aria-label="复制"'))
    assert.ok(result.html.includes('data-fold aria-expanded="false"'))
    assert.ok(result.html.includes('href="https://example.com" target="_blank" rel="noopener noreferrer"'))
    assert.ok(result.html.includes("&lt;script&gt;") && !result.html.includes("<script>"))
    assert.ok(!result.html.includes("<img") && !result.html.includes('href="javascript:'))
    assert.ok(result.html.includes("<table>") && result.html.includes("&lt;tag&gt;"))
  } finally { dispose() }
})
