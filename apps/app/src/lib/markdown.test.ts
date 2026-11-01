import assert from "node:assert/strict"
import test from "node:test"
import { firstLine, renderMarkdown } from "./markdown.ts"

test("markdown never renders raw html, scripts or unsafe links", () => {
  const html = renderMarkdown('<script>alert(1)</script><img src=x onerror=alert(1)>\n\n[a](javascript:alert(1)) [b](https://example.com) ![t](https://tracker.example/p.png)')
  assert.ok(!html.includes("<script") && !html.includes("<img"))
  assert.ok(!html.includes('href="javascript:'))
  assert.ok(html.includes('href="https://example.com" target="_blank" rel="noopener noreferrer"'))
  assert.ok(renderMarkdown("```sh\necho <b>\n```").includes("echo &lt;b&gt;"))
  const lines = (count: number) => renderMarkdown("```\n" + Array.from({ length: count }, (_, i) => i).join("\n") + "\n```")
  assert.ok(!lines(16).includes("data-fold") && lines(17).includes("data-fold"), "only blocks past 16 lines fold")
  assert.ok(renderMarkdown("| a |\n|---|\n| 1 |").includes("<table>"))
})

test("a one-line preview drops markdown marks and stops at the first break", () => {
  assert.equal(firstLine("找到原因：**visualViewport** 没有跟随 `body`\n\n改动：…"), "找到原因：visualViewport 没有跟随 body")
  assert.equal(firstLine("## 标题\n正文"), "标题")
})
