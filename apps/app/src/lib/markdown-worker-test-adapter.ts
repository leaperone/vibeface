import { parentPort, Worker } from "node:worker_threads"
import { installMarkdownWorker } from "./markdown-worker.ts"
import type { MarkdownRequest } from "./markdown-protocol.ts"
import type { MarkdownWorker } from "./markdown-scheduler.ts"

// Import the actual worker handler. The adapter changes only the transport.
if (parentPort) {
  const port = parentPort
  installMarkdownWorker({ listen: (receive) => port.on("message", receive), postMessage: (response) => port.postMessage(response) })
}

export function createNodeMarkdownWorker(): MarkdownWorker {
  const thread = new Worker(new URL(import.meta.url))
  const transport: MarkdownWorker = {
    postMessage: (request: MarkdownRequest) => thread.postMessage(request),
    terminate: () => { void thread.terminate() },
    onmessage: null, onerror: null, onmessageerror: null,
  }
  thread.on("message", (data: unknown) => transport.onmessage?.({ data }))
  thread.on("error", () => transport.onerror?.())
  thread.on("messageerror", () => transport.onmessageerror?.())
  return transport
}
