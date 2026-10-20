"use client"

import { useState } from "react"
import { BellRing, Info, RotateCw, SendHorizontal, TriangleAlert, X, type LucideIcon } from "lucide-react"
import { ActionCard, StatusPill, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useDevice } from "../identity/context"
import { useWorkspace } from "./context"
import { nodeName } from "./labels"
import { ActionButton } from "./present"
import { newId } from "./storage"
import type { Alert, DecisionResult, Severity } from "./types"

const looks: Record<Severity, { icon: LucideIcon; tone: Tone }> = {
  urgent: { icon: TriangleAlert, tone: "danger" },
  attention: { icon: BellRing, tone: "warning" },
  info: { icon: Info, tone: "info" },
}
const agents: Record<string, string> = { codex: "Codex", claude: "Claude Code" }
type Answer = { text: string } | { choice: number }
const same = (a: Answer, b: Answer) => JSON.stringify(a) === JSON.stringify(b)

/** A triaged session waiting on the person: fixed choices, a short reply, or dismiss. Commands and diffs never show here. */
export function AlertCard({ alert }: { alert: Alert }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const [text, setText] = useState("")
  // An unconfirmed send keeps its id, so trying again only reads back what the session has.
  const [sent, setSent] = useState<{ id: string; answer: Answer; result?: DecisionResult } | null>(null)
  const { icon, tone } = looks[alert.severity]
  const open = alert.resolution === null
  // Only a Codex session has a channel back; a Claude Code session is answered in its own terminal.
  const replyable = open && (alert.choices.length > 0 || alert.agent === "codex")
  const disabled = !workspace.writable || !open
  const send = async (answer: Answer) => {
    const id = sent && same(sent.answer, answer) && sent.result?.status !== "rejected" ? sent.id : newId()
    setSent({ id, answer })
    const result = await workspace.sendDecision(alert, id, answer)
    setSent({ id, answer, result })
    // The device marks the alert decided once the session has the answer.
    if (result.status === "delivered") { setText(""); workspace.announce("已送达") }
  }
  const status = sent?.result?.status
  return <ActionCard
    icon={icon}
    tone={tone}
    title={<span className="flex min-w-0 items-center gap-2"><span className="min-w-0 truncate">{alert.summary}</span>{alert.count > 1 ? <span className="shrink-0 rounded-full bg-background/70 px-1.5 text-meta tabular-nums">×{alert.count}</span> : null}</span>}
    meta={`${agents[alert.agent] ?? alert.agent} · ${nodeName(alert.node, { nodes: workspace.nodes, directory })}`}
    menu={open ? <ActionButton variant="ghost" className="-my-2 size-9 px-0" disabled={disabled} action={() => workspace.resolveAlert(alert, "dismissed")}><X aria-hidden="true" /><span className="sr-only">忽略</span></ActionButton> : <StatusPill tone="neutral">{alert.resolution === "decided" ? "已回复" : "已忽略"}</StatusPill>}
    actions={replyable && alert.choices.length ? alert.choices.map((choice, index) => <ActionButton key={choice.label} variant={index === 0 ? "default" : "secondary"} disabled={disabled} action={() => send({ choice: index })}>{choice.label}</ActionButton>) : undefined}
  >
    {alert.cwd ? <p className="truncate font-mono text-meta text-muted-foreground" title={alert.cwd}>{alert.cwd}</p> : null}
    {replyable && !alert.choices.length ? <form className="flex gap-2" onSubmit={(event) => { event.preventDefault(); if (!disabled && text.trim()) void workspace.run(() => send({ text })) }}>
      <Input aria-label="回复" placeholder="回复这个会话" value={text} maxLength={4000} disabled={disabled} onChange={(event) => setText(event.target.value)} />
      <Button type="submit" size="icon" aria-label="发送" title="发送" disabled={disabled || !text.trim()}><SendHorizontal aria-hidden="true" /></Button>
    </form> : null}
    {status === "unknown" && sent ? <p role="status" className="flex items-center gap-2 text-meta text-warning">没确认送达
      <ActionButton variant="ghost" disabled={disabled} action={() => send(sent.answer)}><RotateCw aria-hidden="true" />再试</ActionButton>
    </p> : status === "rejected" ? <p role="alert" className="text-meta text-danger">没送到：{sent?.result?.reason || "会话拒绝了"}</p> : null}
  </ActionCard>
}
