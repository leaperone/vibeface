import { renderMarkdown } from "./markdown.ts"
import { parseMarkdownRequest, type MarkdownResponse } from "./markdown-protocol.ts"

/** The browser worker and the Node worker_threads adapter install this same handler. */
export function installMarkdownWorker(port: {
  listen: (receive: (value: unknown) => void) => void
  postMessage: (response: MarkdownResponse) => void
}) {
  port.listen((value) => {
    const request = parseMarkdownRequest(value)
    if (!request) return
    try {
      port.postMessage({ kind: "rendered", id: request.id, html: renderMarkdown(request.text) })
    } catch {
      port.postMessage({ kind: "failed", id: request.id })
    }
  })
}

if (typeof self !== "undefined" && typeof document === "undefined") {
  installMarkdownWorker({
    listen: (receive) => self.addEventListener("message", (event: MessageEvent<unknown>) => receive(event.data)),
    postMessage: (response) => self.postMessage(response),
  })
}
