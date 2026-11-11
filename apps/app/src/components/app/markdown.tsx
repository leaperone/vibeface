import { memo, useEffect, useMemo, useRef, useState } from "react"
import { renderMarkdown } from "@/lib/markdown"
import { markdownScheduler, type MarkdownResult } from "@/lib/markdown-scheduler"
import { cn } from "@/lib/utils"
import { toast } from "./kit"

/** Bot replies. Raw HTML is never rendered. */
export const Markdown = memo(function Markdown({ text, className, streaming = false }: { text: string; className?: string; streaming?: boolean }) {
  const synchronous = !streaming && text.length <= 4096
  const html = useMemo(() => synchronous ? renderMarkdown(text) : null, [text, synchronous])
  const [rendered, setRendered] = useState<Extract<MarkdownResult, { kind: "rendered" }> | null>(null)
  const [retry, setRetry] = useState(0)
  const session = useRef<ReturnType<typeof markdownScheduler.connect> | null>(null)
  useEffect(() => {
    const connected = markdownScheduler.connect((result) => {
      if (result.kind === "busy") return setRetry((value) => value + 1)
      setRetry(0)
      setRendered(result.kind === "rendered" ? result : null)
    })
    session.current = connected
    return () => {
      session.current = null
      connected.dispose()
    }
  }, [])
  useEffect(() => {
    if (synchronous) session.current?.cancel()
    else session.current?.update(text, streaming)
  }, [text, streaming, synchronous])
  useEffect(() => {
    if (!retry || synchronous) return
    const timer = setTimeout(() => session.current?.update(text, streaming), 120)
    return () => clearTimeout(timer)
  }, [retry, text, streaming, synchronous])
  // A pending parse remains readable, including the newest suffix. React escapes the fallback.
  const parsed = html ?? (rendered && text.startsWith(rendered.text) ? rendered.html : null)
  const tail = html !== null ? "" : rendered && parsed !== null ? text.slice(rendered.text.length) : text
  return <div
    className={cn("md", className)}
    onClick={(event) => {
      if (!(event.target instanceof Element)) return
      const fold = event.target.closest("button[data-fold]")
      if (fold) return fold.setAttribute("aria-expanded", String(fold.getAttribute("aria-expanded") !== "true"))
      const button = event.target.closest("button[data-copy]")
      const code = button?.closest(".md-code")?.querySelector("code")?.textContent
      if (code != null) void navigator.clipboard?.writeText(code).then(() => toast("已复制", { tone: "success" }))
    }}
  >
    {parsed !== null ? <div className="md" dangerouslySetInnerHTML={{ __html: parsed }} /> : null}
    {tail ? <span className="whitespace-pre-wrap break-words">{tail}</span> : null}
  </div>
})

/** User messages: only inline code and line breaks. */
export function PlainText({ text }: { text: string }) {
  return <span className="whitespace-pre-wrap break-words">{text.split(/(`[^`\n]+`)/).map((part, index) => part.length > 2 && part.startsWith("`") && part.endsWith("`")
    ? <code key={index} className="rounded bg-current/15 px-1 font-mono text-[0.9em]">{part.slice(1, -1)}</code>
    : part)}</span>
}
