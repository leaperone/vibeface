"use client"

import { useState, type ReactNode } from "react"
import { Link } from "@tanstack/react-router"
import { Bot, ChevronRight, CircleAlert, CircleCheck, FolderOpen, Info, Laptop, MessageCircleQuestionMark, RotateCw, SendHorizontal, SquareTerminal, TriangleAlert, type LucideIcon } from "lucide-react"
import { ActionCard, BusyIcon, InlineError, useAction, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { useDevice } from "../identity/context"
import { useWorkspace } from "./context"
import { nodeName } from "./labels"
import { agentName, projectName } from "./questions"
import type { Question, Severity } from "./types"

const looks: Record<Severity, { icon: LucideIcon; tone: Tone; label?: string }> = {
  urgent: { icon: TriangleAlert, tone: "danger", label: "紧急" },
  attention: { icon: MessageCircleQuestionMark, tone: "warning" },
  info: { icon: Info, tone: "info" },
}

/**
 * A question waiting on the person, in its chat and in 待处理: tap a choice or type an answer. Answered, it is one line.
 * `answer`: what the chat already holds for it. `inbox`: also names the assistant and links to its chat.
 */
export function QuestionCard({ question, answer, inbox = false }: { question: Question; answer?: string; inbox?: boolean }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const action = useAction()
  const [text, setText] = useState("")
  const id = question.question_id
  const settled = answer ?? question.answer?.text ?? workspace.answered[id]
  if (settled !== undefined) return <p role={answer === undefined && !question.answer ? "status" : undefined} className="flex min-h-9 min-w-0 items-center gap-2 px-2 text-meta text-muted-foreground">
    <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" />
    <span className="min-w-0 flex-1 truncate" title={question.text}>{question.text}</span>
    <span className="max-w-[50%] shrink-0 truncate text-foreground" title={settled}>已回答 · {settled}</span>
  </p>

  const pending = workspace.pendingAnswers.find((item) => item.question_id === id)
  const { icon, tone, label } = looks[question.severity]
  const disabled = !workspace.writable || action.busy !== null
  const session = question.conversation_id === null
  const send = (key: string, value: string) => action.run(key, async () => {
    const result = await workspace.answer(question, value)
    if (result.status === "already_answered" && result.text !== value.trim()) workspace.announce("这个问题已经回答过了")
  })
  return <ActionCard
    icon={icon}
    tone={tone}
    title={<>{label ? <span className="sr-only">{label}：</span> : null}<span className="whitespace-pre-wrap [overflow-wrap:anywhere]">{question.text}</span></>}
  >
    <p className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-meta text-muted-foreground">
      {session ? <Meta icon={SquareTerminal} label="会话">{agentName(question.about.agent)}</Meta> : null}
      <Meta icon={FolderOpen} label="项目" title={question.about.cwd}>{projectName(question.about.cwd)}</Meta>
      <Meta icon={Laptop} label="设备">{nodeName(question.about.node, { nodes: workspace.nodes, directory })}</Meta>
      {inbox && question.assistant ? <Link to="/b/$botId" params={{ botId: question.assistant.bot_id }} className="inline-flex min-w-0 items-center gap-1 rounded-md font-medium text-foreground underline-offset-4 hover:underline max-md:min-h-11">
        <Bot aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">去对话 </span><span className="truncate">{question.assistant.name}</span><ChevronRight aria-hidden="true" className="size-4 shrink-0" />
      </Link> : null}
    </p>
    {session && question.about.summary ? <p className="line-clamp-2 text-muted-foreground [overflow-wrap:anywhere]" title={question.about.summary}>{question.about.summary}</p> : null}
    {pending ? <p role="status" className="flex min-w-0 flex-wrap items-center gap-x-2 text-meta text-muted-foreground">
      <CircleAlert aria-hidden="true" className="size-4 shrink-0 text-warning" />
      <span className="min-w-0 truncate text-foreground">{pending.text}</span>
      <span aria-hidden="true">·</span>没确认送达<span aria-hidden="true">·</span>
      <Button type="button" variant="ghost" disabled={disabled} aria-busy={action.busy === "retry" || undefined} onClick={() => void send("retry", pending.text)}><BusyIcon busy={action.busy === "retry"} icon={RotateCw} />再试</Button>
    </p> : <>
      {question.choices.length ? <div className="flex flex-wrap gap-2">
        {question.choices.map((choice) => {
          const key = `choice:${choice.label}`
          return <Button key={choice.label} type="button" variant="outline" disabled={disabled} aria-busy={action.busy === key || undefined} onClick={() => void send(key, choice.label)}><BusyIcon busy={action.busy === key} />{choice.label}</Button>
        })}
      </div> : null}
      <form className="flex min-w-0 gap-2" onSubmit={(event) => { event.preventDefault(); if (!disabled && text.trim()) void send("text", text) }}>
        <Input aria-label="回答" placeholder="输入回答" className="h-9 bg-background max-md:h-11" value={text} maxLength={4000} disabled={disabled} onChange={(event) => setText(event.target.value)} />
        <Button type="submit" variant="outline" size="icon" aria-label="发送回答" title="发送回答" disabled={disabled || !text.trim()} aria-busy={action.busy === "text" || undefined}><BusyIcon busy={action.busy === "text"} icon={SendHorizontal} /></Button>
      </form>
    </>}
    <InlineError error={pending ? null : action.error} />
  </ActionCard>
}

function Meta({ icon: Icon, label, title, children }: { icon: LucideIcon; label: string; title?: string; children: ReactNode }) {
  return <span className="inline-flex min-w-0 items-center gap-1" title={title}>
    <Icon aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">{label} </span><span className="truncate">{children}</span>
  </span>
}
