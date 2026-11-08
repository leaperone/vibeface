"use client"

import { useRef, useState, type ReactNode } from "react"
import { Link, useNavigate } from "@tanstack/react-router"
import { ChevronRight, CircleAlert, CircleCheck, CircleSlash, CircleX, FolderOpen, Info, Laptop, ListChecks, MessageCircleQuestionMark, RotateCw, SendHorizontal, TriangleAlert, X, type LucideIcon } from "lucide-react"
import { ActionCard, BusyIcon, InlineError, claimFocus, toast, useAction, type Tone } from "@/components/app/kit"
import { BotFace } from "@/components/app/bot-face"
import { terms } from "@/lib/glossary"
import { cn } from "@/lib/utils"
import { Button } from "@/components/ui/button"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { nodeName } from "@vibeface/client/workspace/labels.ts"
import { answerLabel, attentionLabel, approvalTask, declined, ignoredAnswer, questionMeta, settledAnswer, sharedProject, questionParts } from "@vibeface/client/workspace/questions.ts"
import { providerNames } from "@vibeface/client/lib/node.ts"
import { providerIcons } from "@/features/devices/ai-settings"
import type { Question, Severity } from "@vibeface/client/workspace/types.ts"

const looks: Record<Severity, { icon: LucideIcon; tone: Tone; label?: string }> = {
  urgent: { icon: TriangleAlert, tone: "warning", label: "紧急" },
  attention: { icon: MessageCircleQuestionMark, tone: "warning" },
  info: { icon: Info, tone: "info" },
}

/**
 * A question waiting on the person: a choice sends on tap, the last line takes a written answer, and 忽略 sets it aside.
 * `answer`: what the chat already holds for it. `inbox`: also names the bot and links to its chat. `inTask`: inside the task's own panel, which already names the task and device.
 */
export function QuestionCard({ question, answer, inbox = false, inTask = false, from }: { question: Question; answer?: string; inbox?: boolean; inTask?: boolean; from?: "inbox" | "tasks" }) {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
  const action = useAction()
  const navigate = useNavigate()
  const [text, setText] = useState("")
  // Answered here, the result line takes focus after the controls disappear.
  const answering = useRef(false)
  const id = question.question_id
  const inputTarget = question.target
  const taskScope = inputTarget?.kind === 'task' || inputTarget?.kind === 'channel_task' ? inputTarget.task_id : null
  const standing = question.answer?.text ?? answer
  const settled = settledAnswer(standing, workspace.answered[id], question.answer?.author, ready?.principal.device_id)
  const displayAnswer = (value: string) => { const choice = question.choices.find((choice) => choice.label === value); return choice?.action === 'allow_once' ? '允许本次' : choice?.action === 'deny' ? '拒绝' : value }
  const approvalChoices = question.choices.filter((choice) => choice.action === 'allow_once' || choice.action === 'deny')
  // The Node takes only an approval's own actions for a live request; 拒绝 clears it there.
  const ignorable = !approvalChoices.length && !(question.attention?.kind === "approval" && question.about.request_id)
  const { ask, detail } = questionParts(question.text)
  const node = nodeName(question.about.node, { nodes: workspace.nodes, directory })
  // Read back later, an answered approval says which task on which device it settled.
  const heldTask = inTask ? null : approvalTask(question, [...workspace.openWorks, ...(workspace.data?.works ?? [])])
  const about = heldTask ? <span className="flex min-w-0 items-center gap-1"><ListChecks aria-hidden="true" className="size-3.5 shrink-0" /><span className="sr-only">任务 </span><span className="min-w-0 truncate">{heldTask}</span>
    <span aria-hidden="true">·</span><Laptop aria-hidden="true" className="size-3.5 shrink-0" /><span className="sr-only">设备 </span><span className="min-w-0 truncate">{node}</span></span> : null
  if (settled) return <Settled ask={ask} detail={detail} about={about} focus={answering.current} role={standing === undefined || settled.beaten ? "status" : undefined}
    mark={settled.beaten ? <CircleAlert aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-warning" /> : settled.text.trim() === ignoredAnswer ? <CircleSlash aria-hidden="true" className="mt-0.5 size-4 shrink-0" /> : declined(displayAnswer(settled.text)) ? <CircleX aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-danger" /> : <CircleCheck aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-success" />}>
    <span className="text-foreground">{settled.elsewhere ? `已由另一端回答：${displayAnswer(settled.text)}` : answerLabel(displayAnswer(settled.text))}</span>
  </Settled>

  if (question.closed) return <Settled ask={ask} detail={detail} about={about} mark={<CircleSlash aria-hidden="true" className="mt-0.5 size-4 shrink-0" />}>已失效</Settled>

  const pending = workspace.pendingAnswers.find((item) => item.question_id === id)
  const { tone, label, icon: severityIcon } = looks[question.severity]
  const { approval, task, local } = questionMeta(question, workspace.openWorks, workspace.bots, ready?.entry.device_id)
  const icon = approval ? terms.approval.icon : severityIcon
  const disabled = !workspace.writable || action.busy !== null
  const project = question.about.cwd ? sharedProject(question.about.cwd, question.about.node, workspace.nodes) : null
  const send = (key: string, value: string) => {
    if (disabled || pending && value !== pending.text) return
    return action.run(key, async () => {
      answering.current = true
      const result = await workspace.answer(question, value)
      if (result.status === "already_answered" && result.text !== value.trim()) workspace.announce("这个问题已经回答过了")
      else if (inbox) settledToast(value)
    })
  }
  // Answered in 待处理, the card leaves; the toast leads to the chat the answer went to.
  const settledToast = (value: string) => {
    const bot = question.bot
    toast(value === ignoredAnswer ? "已忽略" : "已回答", { tone: "success", action: inputTarget && (taskScope || bot) ? { label: "查看", onClick: () => {
      if (taskScope) void navigate({ to: '/tasks/$taskId', params: { taskId: taskScope } })
      else if (inputTarget?.kind === 'channel_member') void navigate({ to: '/channels/$channelId', params: { channelId: inputTarget.channel_id } })
      else if (bot && inputTarget?.kind === 'work') void navigate({ to: '/b/$botId/work/$workId', params: { botId: bot.bot_id, workId: inputTarget.work_id } })
      else if (bot) void navigate({ to: "/b/$botId", params: { botId: bot.bot_id }, ...(inputTarget?.kind === 'routine' ? { search: { routine: inputTarget.trigger } } : {}) })
    } } : undefined })
  }
  return <ActionCard
    icon={icon}
    tone={tone}
    title={<>{label ? <span className="sr-only">{label}：</span> : null}<span className="[overflow-wrap:anywhere]">{ask}</span></>}
  >
    <p className="text-meta text-muted-foreground">{attentionLabel(question)}{question.attention?.device_id ? ` · 处理设备 ${nodeName(question.attention.device_id, { nodes: workspace.nodes, directory })}` : ''}</p>
    {question.attention?.kind === 'browser_takeover' ? <p className="text-meta text-muted-foreground">请在运行设备的浏览器中完成操作，确认结果后再回来回答。</p> : null}
    <Lines text={detail} />
    {inTask ? null : <p className="flex min-w-0 items-center gap-3 text-meta text-muted-foreground">
      {taskScope ? <Link to="/tasks/$taskId" params={{ taskId: taskScope }} className="inline-flex min-h-11 items-center gap-1 underline underline-offset-4"><ListChecks aria-hidden="true" className="size-4" />打开所属任务</Link> : null}
      {task ? <Link to="/b/$botId/work/$workId" params={{ botId: task.bot_id, workId: task.work_id }} search={from === "tasks" ? { from: "tasks" } : from === "inbox" || inbox ? { from: "inbox" } : {}} className="inline-flex min-w-12 items-center gap-1 rounded-md underline-offset-4 hover:text-foreground hover:underline max-md:min-h-11">
        <ListChecks aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">任务 </span><span className="truncate">{task.title}</span>
      </Link> : null}
      {question.about.agent ? <Meta icon={providerIcons[question.about.agent]} label="来自" compact={Boolean(task)}>{providerNames[question.about.agent]}</Meta> : null}
      {project ? <Meta icon={FolderOpen} label="项目" title={project.path} compact={Boolean(task)}>{project.name}</Meta> : null}
      {/* As on task rows, only another device is named on screen. */}
      {local ? <span className="sr-only">设备 {node}</span>
        : <Meta icon={Laptop} label="设备" title={node} compact={Boolean(task)}>{node}</Meta>}
      {inbox && question.bot && inputTarget && !taskScope ? <Link to="/b/$botId" params={{ botId: question.bot.bot_id }} search={inputTarget?.kind === "routine" ? { routine: inputTarget.trigger } : {}} title={question.bot.name} className="ms-auto inline-flex min-w-0 shrink-[2] items-center justify-end gap-1.5 rounded-md font-medium text-foreground underline-offset-4 hover:underline max-md:min-h-11 max-md:min-w-11">
        <BotFace bot={workspace.bots.find((item) => item.bot_id === question.bot?.bot_id) ?? { bot_id: question.bot.bot_id, avatar: null }} mood="idle" size={16} /><span className="sr-only">去对话 </span><span className="truncate max-sm:sr-only">{question.bot.name}</span><ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
      </Link> : null}
    </p>}
    {pending ? <p role="status" className="flex min-w-0 flex-wrap items-center gap-x-2 text-meta text-muted-foreground">
      <CircleAlert aria-hidden="true" className="size-4 shrink-0 text-warning" />
      <span className="min-w-0 whitespace-pre-wrap [overflow-wrap:anywhere] text-foreground">{displayAnswer(pending.text)}</span>
      <span aria-hidden="true">·</span>{action.busy !== null ? "正在发送" : "没确认送达"}<span aria-hidden="true">·</span>
      <Button type="button" variant="ghost" disabled={disabled} aria-busy={action.busy === "retry" || undefined} onClick={() => void send("retry", pending.text)}><BusyIcon busy={action.busy === "retry"} icon={RotateCw} />再试</Button>
    </p> : <div className="grid gap-2">
      {approvalChoices.length ? <div className="flex flex-wrap gap-2">{approvalChoices.map((choice) => <Button key={choice.label} type="button" variant={choice.action === 'allow_once' ? 'default' : 'outline'} className="min-h-11" disabled={disabled} aria-busy={action.busy === `permission:${choice.action}` || undefined} onClick={() => void send(`permission:${choice.action}`, choice.label)}><BusyIcon busy={action.busy === `permission:${choice.action}`} icon={choice.action === 'allow_once' ? CircleCheck : CircleX} />{choice.action === 'allow_once' ? '允许本次' : '拒绝'}</Button>)}</div> : <>
        {question.choices.length ? <div role="group" aria-label="选一个回答" className="grid min-w-0 gap-2">
          {question.choices.map((choice, index) => <button key={index} type="button" className={row} disabled={disabled} aria-busy={action.busy === `choice:${index}` || undefined} onClick={() => void send(`choice:${index}`, choice.label)}>
            <span aria-hidden="true" className="w-4 shrink-0 text-center tabular-nums text-muted-foreground">{action.busy === `choice:${index}` ? <BusyIcon busy /> : index + 1}</span>
            <span className="min-w-0 flex-1 whitespace-pre-wrap [overflow-wrap:anywhere]">{choice.label}</span>
          </button>)}
        </div> : null}
        <div className="flex min-w-0 items-end gap-2">
          <textarea aria-label="你的回答" rows={1} placeholder={question.choices.length ? "或者自己写回答" : "写下你的回答"} className="max-h-60 min-h-11 min-w-0 flex-1 resize-none rounded-lg border bg-card px-3 py-2.5 text-body outline-none [field-sizing:content] placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50" value={text} maxLength={4000} disabled={disabled} onChange={(event) => setText(event.target.value)} />
          <Button type="button" size="icon" className="size-11 shrink-0" aria-label="发送回答" disabled={disabled || !text.trim()} aria-busy={action.busy === "answer" || undefined} onClick={() => void send("answer", text)}><BusyIcon busy={action.busy === "answer"} icon={SendHorizontal} /></Button>
        </div>
      </>}
      {ignorable ? <Button type="button" variant="ghost" className="min-h-11 justify-self-start text-muted-foreground" disabled={disabled} aria-busy={action.busy === "ignore" || undefined} onClick={() => void send("ignore", ignoredAnswer)}><BusyIcon busy={action.busy === "ignore"} icon={X} />忽略</Button> : null}
    </div>}
    <InlineError error={action.error} />
  </ActionCard>
}

function Lines({ text, className }: { text: string; className?: string }) {
  return text ? <p className={cn("whitespace-pre-wrap [overflow-wrap:anywhere] text-body", className)}>{text}</p> : null
}

function Settled({ ask, detail, about, mark, focus = false, role, children }: { ask: string; detail: string; about?: ReactNode; mark: ReactNode; focus?: boolean; role?: "status"; children: ReactNode }) {
  const line = <span role={role} className="grid min-w-0 flex-1 gap-0.5">
    {about}
    <span className="flex min-w-0 flex-col gap-0.5 sm:flex-row sm:gap-2">
      <span className="min-w-0 truncate sm:max-w-[40%] sm:shrink-0" title={ask}>{ask}</span>
      <span className="min-w-0 flex-1 [overflow-wrap:anywhere]">{children}</span>
    </span>
  </span>
  const settled = "flex min-h-9 min-w-0 items-start gap-2 px-2 py-2 outline-none"
  if (!detail) return <p ref={focus ? claimFocus : undefined} tabIndex={-1} className={cn("-mx-2 text-meta text-muted-foreground", settled)}>{mark}{line}</p>
  return <details className="group -mx-2 text-meta text-muted-foreground">
    <summary ref={focus ? claimFocus : undefined} className={cn(settled, "cursor-pointer list-none rounded-md hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50 [&::-webkit-details-marker]:hidden")}>
      {mark}{line}<ChevronRight aria-hidden="true" className="mt-0.5 size-4 shrink-0 transition-transform duration-200 group-open:rotate-90" />
    </summary>
    <Lines text={detail} className="ps-8 pe-2 pb-2" />
  </details>
}

const row = "flex min-h-12 [&_svg]:size-4 w-full min-w-0 cursor-pointer items-start gap-3 rounded-lg border bg-card px-3 py-3 text-start font-medium transition-colors hover:bg-muted outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50"

/** `compact`: on a phone it gives way to the task link, so the line keeps to one row. */
function Meta({ icon: Icon, label, title, compact = false, children }: { icon: LucideIcon; label: string; title?: string; compact?: boolean; children: ReactNode }) {
  return <span className={compact ? "inline-flex min-w-0 items-center gap-1 max-sm:hidden" : "inline-flex min-w-0 items-center gap-1"} title={title}>
    <Icon aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">{label} </span><span className="truncate">{children}</span>
  </span>
}
