import assert from "node:assert/strict"
import { createRequire } from "node:module"
import { fileURLToPath } from "node:url"
import { createContext } from "node:vm"
import test from "node:test"
import * as React from "react"
import { renderToStaticMarkup } from "react-dom/server"
import { renderMarkdown } from "./markdown.ts"
import { moduleLoader } from "./test-harness.ts"

const require = createRequire(import.meta.url)

function render(props: { text: string; streaming?: boolean }) {
  let parses = 0
  const load = moduleLoader(createContext({}), (specifier) => {
    if (specifier === "react") return { ...React, memo: (component: unknown) => component }
    if (specifier === "@/lib/markdown") return { renderMarkdown: (text: string) => { parses++; return renderMarkdown(text) } }
    if (specifier === "@/lib/markdown-scheduler") return { markdownScheduler: { connect: () => { throw new Error("SSR must not connect a worker") } } }
    if (specifier === "@/lib/utils") return { cn: (...values: unknown[]) => values.filter(Boolean).join(" ") }
    if (specifier === "./kit") return { toast: () => {} }
    return require(specifier)
  })
  const component = load(fileURLToPath(new URL("../components/app/markdown.tsx", import.meta.url))).Markdown
  if (typeof component !== "function") throw new Error("Missing Markdown component")
  const html = renderToStaticMarkup(React.createElement(() => component(props)))
  return { html, parses }
}

test("streaming SSR stays readable and escapes all text without parsing or starting workers", () => {
  const result = render({ text: '<script>alert("x")</script> **still streaming**', streaming: true })
  assert.equal(result.parses, 0)
  assert.ok(result.html.includes('&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; **still streaming**'))
  assert.ok(!result.html.includes("<script>"))
})

test("long SSR fallback preserves the complete message and short final output retains Markdown", () => {
  const text = "<unsafe>\n".repeat(1000) + "END OF COMPLETE MESSAGE"
  const pending = render({ text })
  assert.equal(pending.parses, 0)
  assert.equal(pending.html.match(/&lt;unsafe&gt;/g)?.length, 1000)
  assert.ok(pending.html.includes("END OF COMPLETE MESSAGE"))
  const short = render({ text: "**ready**\n\n```ts\n<tag>\n```" })
  assert.equal(short.parses, 1)
  assert.ok(short.html.includes("<strong>ready</strong>"))
  assert.ok(short.html.includes('data-copy aria-label="复制"'))
  assert.ok(short.html.includes("&lt;tag&gt;"))
})
