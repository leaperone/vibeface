import { useMemo } from "react"
import { renderMarkdown } from "@/lib/markdown"
import { cn } from "@/lib/utils"
import { toast } from "./kit"

/** Bot replies. Raw HTML is never rendered. */
export function Markdown({ text, className }: { text: string; className?: string }) {
  const html = useMemo(() => renderMarkdown(text), [text])
  return <div
    className={cn("md", className)}
    dangerouslySetInnerHTML={{ __html: html }}
    onClick={(event) => {
      const fold = (event.target as HTMLElement).closest("button[data-fold]")
      if (fold) return fold.setAttribute("aria-expanded", String(fold.getAttribute("aria-expanded") !== "true"))
      const button = (event.target as HTMLElement).closest("button[data-copy]")
      const code = button?.closest(".md-code")?.querySelector("code")?.textContent
      if (code != null) void navigator.clipboard?.writeText(code).then(() => toast("已复制", { tone: "success" }))
    }}
  />
}

/** User messages: only inline code and line breaks. */
export function PlainText({ text }: { text: string }) {
  return <span className="whitespace-pre-wrap break-words">{text.split(/(`[^`\n]+`)/).map((part, index) => part.length > 2 && part.startsWith("`") && part.endsWith("`")
    ? <code key={index} className="rounded bg-current/15 px-1 font-mono text-[0.9em]">{part.slice(1, -1)}</code>
    : part)}</span>
}
