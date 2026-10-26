"use client"

import { useRef, useState, type ReactNode } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { ChevronRight, CircleAlert, CircleCheck, CircleSlash, FolderOpen, Info, ListChecks, LoaderCircle, MessageCircleQuestionMark, PencilLine, RotateCw, SendHorizontal, Server, TriangleAlert, type LucideIcon } from "lucide-react"
import { ActionCard, BotAvatar, BusyIcon, InlineError, claimFocus, toast, useAction, type Tone } from "@/components/app/kit"
import { terms } from "@/lib/glossary"
import { cn } from "@/lib/utils"
import { Button } from "@/components/ui/button"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { nodeName } from "@vibeface/client/workspace/labels.ts"
import { projectName, settledAnswer } from "@vibeface/client/workspace/questions.ts"
import type { Question, Severity } from "@vibeface/client/workspace/types.ts"

const looks: Record<Severity, { icon: LucideIcon; tone: Tone; label?: string }> = {
  urgent: { icon: TriangleAlert, tone: "warning", label: "紧急" },
  attention: { icon: MessageCircleQuestionMark, tone: "warning" },
  info: { icon: Info, tone: "info" },
}

/**
 * A question waiting on the person, in its chat and in 待处理: tap a choice or type an answer. Answered or closed, it is one line.
 * `answer`: what the chat already holds for it. `inbox`: also names the bot and links to its chat. `inTask`: inside the task's own panel, which already names the task and device.
 */
export function QuestionCard({ question, answer, inbox = false, inTask = false }: { question: Question; answer?: string; inbox?: boolean; inTask?: boolean }) {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
  const action = useAction()
  const navigate = useNavigate()
  const [text, setText] = useState("")
  // Answered here, the card turns into one line and the pressed choice goes with it; that line takes the focus.
  const answering = useRef(false)
  const id = question.question_id
  const standing = answer ?? question.answer?.text
  const settled = settledAnswer(standing, workspace.answered[id])
  // The first line asks; lines after it list what the answer applies to.
  const [ask = question.text, ...lines] = question.text.split(/\r?\n/).filter((line) => line.trim())
  if (settled) return <Settled ask={ask} lines={lines} focus={answering.current} role={standing === undefined || settled.elsewhere ? "status" : undefined}
    mark={settled.elsewhere ? <CircleAlert aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-warning" /> : <CircleCheck aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-success" />}>
    <span className="text-foreground">{settled.elsewhere ? "已由另一端回答：" : "已回答 · "}{settled.text}</span>
  </Settled>

  if (question.closed) return <Settled ask={ask} lines={lines} mark={<CircleSlash aria-hidden="true" className="mt-0.5 size-4 shrink-0" />}>已失效</Settled>

  const pending = workspace.pendingAnswers.find((item) => item.question_id === id)
  const { tone, label, icon: severityIcon } = looks[question.severity]
  // A held App Server request reads as 需要你同意 wherever it shows, and names the task it holds.
  const approval = Boolean(question.about.request_id)
  const icon = approval ? terms.approval.icon : severityIcon
  const task = approval ? workspace.openWorks.find((work) => work.thread_id === question.about.session_id && !work.outcome) : undefined
  const taskBot = task ? workspace.bots.find((bot) => bot.conversation_id === task.conversation_id)?.bot_id : undefined
  const disabled = !workspace.writable || action.busy !== null
  const shown = question.choices
  const node = nodeName(question.about.node, { nodes: workspace.nodes, directory })
  const send = (key: string, value: string) => action.run(key, async () => {
    answering.current = true
    const result = await workspace.answer(question, value)
    if (result.status === "already_answered" && result.text !== value.trim()) workspace.announce("这个问题已经回答过了")
    else if (inbox) settledToast()
  })
  // Answered in 待处理, the card leaves; the toast leads to the chat the answer went to.
  const settledToast = () => {
    const bot = question.bot
    toast("已回答", { tone: "success", action: bot ? { label: "查看", onClick: () => { void navigate({ to: "/b/$botId", params: { botId: bot.bot_id } }) } } : undefined })
  }
  return <ActionCard
    icon={icon}
    tone={tone}
    title={<>{label ? <span className="sr-only">{label}：</span> : null}<span className="[overflow-wrap:anywhere]">{ask}</span></>}
  >
    <Lines lines={lines} />
    {inTask ? null : <p className="flex min-w-0 items-center gap-3 text-meta text-muted-foreground">
      {task && taskBot ? <Link to="/b/$botId/work/$workId" params={{ botId: taskBot, workId: task.work_id }} search={inbox ? { from: "inbox" } : {}} className="inline-flex min-w-12 items-center gap-1 rounded-md underline-offset-4 hover:text-foreground hover:underline max-md:min-h-11">
        <ListChecks aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">任务 </span><span className="truncate">{task.title || "任务"}</span>
      </Link> : null}
      {question.about.cwd ? <Meta icon={FolderOpen} label="项目" title={question.about.cwd} compact={Boolean(task)}>{projectName(question.about.cwd)}</Meta> : null}
      {/* As on task rows, only another device is named on screen. */}
      {question.about.node === ready?.entry.device_id ? <span className="sr-only">设备 {node}</span>
        : <Meta icon={Server} label="设备" title={node} compact={Boolean(task)}>{node}</Meta>}
      {inbox && question.bot ? <Link to="/b/$botId" params={{ botId: question.bot.bot_id }} title={question.bot.name} className="ms-auto inline-flex min-w-0 shrink-[2] items-center justify-end gap-1.5 rounded-md font-medium text-foreground underline-offset-4 hover:underline max-md:min-h-11 max-md:min-w-11">
        <BotAvatar name={question.bot.name} primary={workspace.primary === question.conversation_id} size="sm" className="size-4 rounded text-[9px]" /><span className="sr-only">去对话 </span><span className="truncate max-sm:sr-only">{question.bot.name}</span><ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
      </Link> : null}
    </p>}
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
          return <button key={choice.label} type="button" className={row} disabled={disabled} aria-busy={action.busy === key || undefined} aria-keyshortcuts={index < 9 ? String(index + 1) : undefined} onClick={() => void send(key, choice.label)}>
            <span aria-hidden="true" className={`${badge} bg-muted text-muted-foreground`}>{action.busy === key ? <LoaderCircle className="size-4 animate-spin" /> : index + 1}</span>
            <span className="min-w-0 flex-1 [overflow-wrap:anywhere]">{choice.label}</span>
          </button>
        })}
      </div> : null}
      <form className="flex min-h-12 min-w-0 items-center gap-3 rounded-lg border bg-card ps-3 pe-1 transition-colors focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50 has-[input:disabled]:opacity-50" onSubmit={(event) => {
        event.preventDefault()
        if (disabled || !text.trim()) return
        // A typed option number picks that option, as tapping its row would.
        const typed = text.trim().replace(/^[１-９]$/, (digit) => String(digit.charCodeAt(0) - 0xff10))
        const numbered = /^[1-9]$/.test(typed) ? shown[Number(typed) - 1] : undefined
        void (numbered ? send(`choice:${numbered.label}`, numbered.label) : send("text", text))
      }}>
        <span aria-hidden="true" className={`${badge} bg-muted text-muted-foreground`}><PencilLine className="size-4" /></span>
        <input aria-label="其他回答" placeholder={shown.length ? "其他回答" : "输入回答"} className="h-11 min-w-0 flex-1 bg-transparent text-base outline-none placeholder:text-muted-foreground disabled:cursor-not-allowed md:text-sm" value={text} maxLength={4000} disabled={disabled} onChange={(event) => setText(event.target.value)} />
        <Button type="submit" variant="ghost" size="icon" className="size-10 max-md:size-11" aria-label="发送回答" title="发送回答" disabled={disabled || !text.trim()} aria-busy={action.busy === "text" || undefined}><BusyIcon busy={action.busy === "text"} icon={SendHorizontal} /></Button>
      </form>
    </div>}
    <InlineError error={pending ? null : action.error} />
  </ActionCard>
}

/** What an answer applies to, one line each: a label before "：" and its value. */
function Lines({ lines, className }: { lines: string[]; className?: string }) {
  if (!lines.length) return null
  return <ul className={cn("grid gap-1", className)}>{lines.map((line, index) => {
    const colon = line.indexOf("：")
    return <li key={index} className="flex min-w-0 flex-wrap gap-x-2 [overflow-wrap:anywhere]">
      {colon > 0 ? <><span className="text-muted-foreground">{line.slice(0, colon)}</span><span className="min-w-0 font-medium text-foreground">{line.slice(colon + 1)}</span></> : <span className="font-medium text-foreground">{line}</span>}
    </li>
  })}</ul>
}

/**
 * An answered or closed question as one line. With lines of detail, tapping the line opens them, so a phone without hover still sees what
 * the answer applied to. `focus`: answered here, the line takes the focus. `role`: "status" to announce it when it appears.
 */
function Settled({ ask, lines, mark, focus = false, role, children }: { ask: string; lines: string[]; mark: ReactNode; focus?: boolean; role?: "status"; children: ReactNode }) {
  const line = <span role={role} className="flex min-w-0 flex-1 flex-col gap-0.5 sm:flex-row sm:gap-2">
    <span className="min-w-0 truncate sm:max-w-[40%] sm:shrink-0">{ask}</span>
    <span className="min-w-0 flex-1 [overflow-wrap:anywhere]">{children}</span>
  </span>
  const settled = "flex min-h-9 min-w-0 items-start gap-2 px-2 py-2 outline-none"
  if (!lines.length) return <p ref={focus ? claimFocus : undefined} tabIndex={-1} className={cn("-mx-2 text-meta text-muted-foreground", settled)}>{mark}{line}</p>
  return <details className="group -mx-2 text-meta text-muted-foreground">
    <summary ref={focus ? claimFocus : undefined} className={cn(settled, "cursor-pointer list-none rounded-md hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50 [&::-webkit-details-marker]:hidden")}>
      {mark}{line}<ChevronRight aria-hidden="true" className="mt-0.5 size-4 shrink-0 transition-transform duration-150 group-open:rotate-90" />
    </summary>
    <Lines lines={lines} className="ps-8 pe-2 pb-2" />
  </details>
}

const row = "flex min-h-12 w-full min-w-0 items-center gap-3 rounded-lg border bg-card px-3 py-2 text-start font-medium transition-colors outline-none hover:bg-muted focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 active:bg-muted disabled:pointer-events-none disabled:opacity-50"
const badge = "grid size-7 shrink-0 place-items-center rounded-md text-meta font-semibold tabular-nums"

/** `compact`: on a phone it gives way to the task link, so the line keeps to one row. */
function Meta({ icon: Icon, label, title, compact = false, children }: { icon: LucideIcon; label: string; title?: string; compact?: boolean; children: ReactNode }) {
  return <span className={compact ? "inline-flex min-w-0 items-center gap-1 max-sm:hidden" : "inline-flex min-w-0 items-center gap-1"} title={title}>
    <Icon aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">{label} </span><span className="truncate">{children}</span>
  </span>
}
