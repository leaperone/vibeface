"use client"

import { useState, type ReactNode } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { Bot, ChevronRight, CircleAlert, CircleCheck, FolderOpen, Info, Laptop, LoaderCircle, MessageCircleQuestionMark, PencilLine, RotateCw, SendHorizontal, SquareTerminal, TriangleAlert, type LucideIcon } from "lucide-react"
import { ActionCard, BusyIcon, InlineError, toast, useAction, type Tone } from "@/components/app/kit"
import { Button } from "@/components/ui/button"
import { useDevice } from "../identity/context"
import { useWorkspace } from "./context"
import { nodeName } from "./labels"
import { agentName, projectName, settledAnswer } from "./questions"
import type { Question, Severity } from "./types"

const looks: Record<Severity, { icon: LucideIcon; tone: Tone; label?: string }> = {
  urgent: { icon: TriangleAlert, tone: "warning", label: "紧急" },
  attention: { icon: MessageCircleQuestionMark, tone: "warning" },
  info: { icon: Info, tone: "info" },
}

/**
 * A question waiting on the person, in its chat and in 待处理: tap a choice or type an answer. Answered, it is one line.
 * `answer`: what the chat already holds for it. `inbox`: also names the bot and links to its chat.
 */
export function QuestionCard({ question, answer, inbox = false }: { question: Question; answer?: string; inbox?: boolean }) {
  const workspace = useWorkspace()
  const { directory } = useDevice()
  const action = useAction()
  const navigate = useNavigate()
  const [text, setText] = useState("")
  const id = question.question_id
  const standing = answer ?? question.answer?.text
  const settled = settledAnswer(standing, workspace.answered[id])
  if (settled) return <p role={standing === undefined || settled.elsewhere ? "status" : undefined} className="-mx-2 flex min-h-9 min-w-0 items-start gap-2 px-2 py-2 text-meta text-muted-foreground">
    {settled.elsewhere ? <CircleAlert aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-warning" /> : <CircleCheck aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-success" />}
    <span className="flex min-w-0 flex-1 flex-col gap-0.5 sm:flex-row sm:gap-2">
      <span className="min-w-0 truncate sm:max-w-[40%] sm:shrink-0" title={question.text}>{question.text}</span>
      <span className="min-w-0 flex-1 text-foreground [overflow-wrap:anywhere]">{settled.elsewhere ? "已由另一端回答：" : "已回答 · "}{settled.text}</span>
    </span>
  </p>

  const pending = workspace.pendingAnswers.find((item) => item.question_id === id)
  const { icon, tone, label } = looks[question.severity]
  const disabled = !workspace.writable || action.busy !== null
  const session = question.conversation_id === null
  // 交给 the primary bot repeats 处理.
  const shown = question.choices.filter((choice) => !session || !choice.conversation_id || choice.conversation_id !== workspace.primary)
  const send = (key: string, value: string) => action.run(key, async () => {
    const result = await workspace.answer(question, value)
    if (result.status === "already_answered" && result.text !== value.trim()) workspace.announce("这个问题已经回答过了")
    else if (inbox) settledToast(value)
  })
  // Answered in 待处理, the card leaves; the toast says where the work went and leads there.
  const settledToast = (value: string) => {
    if (session && value === "忽略") return toast("已忽略这个会话", { tone: "success" })
    const chosen = session ? question.choices.find((choice) => choice.label === value)?.conversation_id : question.conversation_id
    const bot = workspace.bots.find((item) => item.conversation_id === (chosen ?? workspace.primary))
    toast(!session ? "已回答" : chosen ? `已交给 ${bot?.name ?? "Bot"}` : "已交给主 Bot 分派", {
      tone: "success", action: bot ? { label: "查看", onClick: () => { void navigate({ to: "/b/$botId", params: { botId: bot.bot_id } }) } } : undefined,
    })
  }
  return <ActionCard
    icon={icon}
    tone={tone}
    title={<>{label ? <span className="sr-only">{label}：</span> : null}<span className="whitespace-pre-wrap [overflow-wrap:anywhere]">{question.text}</span></>}
  >
    <p className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-meta text-muted-foreground">
      {session ? <Meta icon={SquareTerminal} label="会话">{agentName(question.about.agent)}</Meta> : null}
      {question.about.cwd ? <Meta icon={FolderOpen} label="项目" title={question.about.cwd}>{projectName(question.about.cwd)}</Meta> : null}
      <Meta icon={Laptop} label="设备">{nodeName(question.about.node, { nodes: workspace.nodes, directory })}</Meta>
      {inbox && question.bot ? <Link to="/b/$botId" params={{ botId: question.bot.bot_id }} className="inline-flex min-w-0 items-center gap-1 rounded-md font-medium text-foreground underline-offset-4 hover:underline max-md:min-h-11">
        <Bot aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">去对话 </span><span className="truncate">{question.bot.name}</span><ChevronRight aria-hidden="true" className="size-4 shrink-0" />
      </Link> : null}
    </p>
    {session && question.about.summary ? <p className="line-clamp-2 text-muted-foreground [overflow-wrap:anywhere]" title={question.about.summary}>{question.about.summary}</p> : null}
    {pending ? <p role="status" className="flex min-w-0 flex-wrap items-center gap-x-2 text-meta text-muted-foreground">
      <CircleAlert aria-hidden="true" className="size-4 shrink-0 text-warning" />
      <span className="min-w-0 truncate text-foreground">{pending.text}</span>
      <span aria-hidden="true">·</span>没确认送达<span aria-hidden="true">·</span>
      <Button type="button" variant="ghost" disabled={disabled} aria-busy={action.busy === "retry" || undefined} onClick={() => void send("retry", pending.text)}><BusyIcon busy={action.busy === "retry"} icon={RotateCw} />再试</Button>
    </p> : <div className="grid gap-2">
      {shown.length ? <div role="group" aria-label="选一个回答" className="grid gap-2" onKeyDown={(event) => {
        // A focused choice row also answers by its number.
        const choice = !event.altKey && !event.ctrlKey && !event.metaKey ? shown[Number(event.key) - 1] : undefined
        if (choice && !disabled) { event.preventDefault(); void send(`choice:${choice.label}`, choice.label) }
      }}>
        {shown.map((choice, index) => {
          const key = `choice:${choice.label}`
          const suggested = session && choice.label === "处理"
          return <button key={choice.label} type="button" className={row} disabled={disabled} aria-busy={action.busy === key || undefined} aria-keyshortcuts={index < 9 ? String(index + 1) : undefined} onClick={() => void send(key, choice.label)}>
            <span aria-hidden="true" className={`${badge} ${suggested ? "bg-primary/10 text-foreground ring-1 ring-primary/25" : "bg-muted text-muted-foreground"}`}>{action.busy === key ? <LoaderCircle className="size-4 animate-spin" /> : index + 1}</span>
            <span className="min-w-0 flex-1 [overflow-wrap:anywhere]">{choice.label}</span>
            {suggested ? <span className="shrink-0 text-meta font-normal text-muted-foreground">主 Bot 分派</span> : null}
          </button>
        })}
      </div> : null}
      <form className="flex min-h-12 min-w-0 items-center gap-3 rounded-lg border bg-background ps-3 pe-1 transition-colors focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50 has-[input:disabled]:opacity-50" onSubmit={(event) => {
        event.preventDefault()
        if (disabled || !text.trim()) return
        // A typed option number picks that option, as tapping its row would.
        const typed = text.trim().replace(/^[１-９]$/, (digit) => String(digit.charCodeAt(0) - 0xff10))
        const numbered = /^[1-9]$/.test(typed) ? shown[Number(typed) - 1] : undefined
        void (numbered ? send(`choice:${numbered.label}`, numbered.label) : send("text", text))
      }}>
        <span aria-hidden="true" className={`${badge} bg-muted text-muted-foreground`}><PencilLine className="size-4" /></span>
        <input aria-label="其他回答" placeholder={shown.length ? "其他回答" : "输入回答"} className="h-11 min-w-0 flex-1 bg-transparent text-base outline-none placeholder:text-muted-foreground disabled:cursor-not-allowed md:text-sm" value={text} maxLength={4000} disabled={disabled} onChange={(event) => setText(event.target.value)} />
        <Button type="submit" variant="ghost" size="icon" className="size-10 max-md:size-10" aria-label="发送回答" title="发送回答" disabled={disabled || !text.trim()} aria-busy={action.busy === "text" || undefined}><BusyIcon busy={action.busy === "text"} icon={SendHorizontal} /></Button>
      </form>
    </div>}
    <InlineError error={pending ? null : action.error} />
  </ActionCard>
}

const row = "flex min-h-12 w-full min-w-0 items-center gap-3 rounded-lg border bg-background px-3 py-2 text-start font-medium transition-colors outline-none hover:bg-muted focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 active:bg-muted disabled:pointer-events-none disabled:opacity-50"
const badge = "grid size-7 shrink-0 place-items-center rounded-md text-meta font-semibold tabular-nums"

function Meta({ icon: Icon, label, title, children }: { icon: LucideIcon; label: string; title?: string; children: ReactNode }) {
  return <span className="inline-flex min-w-0 items-center gap-1" title={title}>
    <Icon aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">{label} </span><span className="truncate">{children}</span>
  </span>
}
