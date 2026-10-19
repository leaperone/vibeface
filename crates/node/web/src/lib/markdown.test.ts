import assert from "node:assert/strict"
import test from "node:test"
import { renderMarkdown } from "./markdown.ts"

test("markdown never renders raw html, scripts or unsafe links", () => {
  const html = renderMarkdown('<script>alert(1)</script><img src=x onerror=alert(1)>\n\n[a](javascript:alert(1)) [b](https://example.com) ![t](https://tracker.example/p.png)')
  assert.ok(!html.includes("<script") && !html.includes("<img"))
  assert.ok(!html.includes('href="javascript:'))
  assert.ok(html.includes('href="https://example.com" target="_blank" rel="noopener noreferrer"'))
  assert.ok(renderMarkdown("```sh\necho <b>\n```").includes("echo &lt;b&gt;"))
  assert.ok(renderMarkdown("| a |\n|---|\n| 1 |").includes("<table>"))
})
