"use client"

import { Bot, CircleQuestionMark, CircleStop, Clock3, Laptop, Send, Server, Unplug } from "lucide-react"
import { Link } from "@tanstack/react-router"
import { Card, CardContent } from "@/components/ui/card"
import { Details, DetailRow, StatusPill, type Tone } from "@/components/app/kit"
import { Message } from "@/features/chat/elements/message"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { nodeName, shownState, taskLabel } from "@vibeface/client/workspace/labels.ts"
import { providerIcons } from "@/features/devices/ai-settings"
import { providerNames } from "@vibeface/client/lib/node.ts"
import { questionOpen } from "@vibeface/client/workspace/questions.ts"
import type { ConversationPayload, ManagerJob, Question, SessionHandoff, SentBotMessage, WorkRecord } from "@vibeface/client/workspace/types.ts"
import { cn } from "@/lib/utils"

/** A read-only view of this conversation's local scheduler and Bot-to-Bot records. */
export type CollaborationPanelProps = { data?: ConversationPayload | null; botId?: string; className?: string }

const schedulerLabels: Record<string, string> = {
  queued: "排队中",
  claimed: "已领取",
  running: "执行中",
  steering: "补充中",
  cancelling: "停止中",
  steered: "已补充",
  completed: "已完成",
  failed: "失败",
  cancelled: "已取消",
  interrupted: "已中断",
  pending_delivery: "等待设备接收",
}

const terminalStates = new Set(["steered", "completed", "failed", "cancelled", "interrupted"])
const activeStates = new Set(["queued", "claimed", "running", "steering", "cancelling"])

function schedulerLabel(state: string) {
  return schedulerLabels[state] ?? "未知状态"
}

function schedulerTone(state: string): Tone {
  if (state === "failed" || state === "interrupted") return "danger"
  if (state === "cancelled") return "warning"
  if (terminalStates.has(state)) return "success"
  if (activeStates.has(state)) return "info"
  return "neutral"
}

function triggerKind(trigger: string) {
  if (trigger.startsWith("bot_message:")) return { label: "Bot 协作消息", detail: "message_bot" }
  if (trigger.startsWith("task:")) return { label: "执行工作", detail: "dispatch_work" }
  if (trigger.startsWith("work:")) return { label: "工作结果", detail: "调度记录" }
  if (trigger.startsWith("schedule:")) return { label: "定时任务", detail: "schedule" }
  if (trigger.startsWith("message:")) return { label: "用户消息", detail: "普通消息" }
  return { label: "调度项", detail: "未知触发" }
}

function when(ms: number) {
  return new Date(ms).toLocaleString("zh-CN", { dateStyle: "medium", timeStyle: "short" })
}

function Section({ title, count, children, className }: { title: string; count?: number; children: React.ReactNode; className?: string }) {
  return <section aria-label={title} className={cn("grid gap-2", className)}>
    <h3 className="flex items-center gap-2 px-1 text-meta font-medium text-muted-foreground">
      <span>{title}</span>{count === undefined ? null : <span className="tabular-nums">· {count}</span>}
    </h3>
    {children}
  </section>
}

function OwnerCard({ data, nodes }: { data: ConversationPayload; nodes: ReturnType<typeof useWorkspace>["nodes"] }) {
  const owner = data.conversation?.owner
  const remote = data.scheduler?.remote
  const remoteOwner = remote?.owner ?? owner
  const ownerNode = remoteOwner ? nodes.find((node) => node.device_id === remoteOwner) : undefined
  const reachable = remote
    ? remote.reachable
    : owner === data.local_node
      ? true
      : ownerNode?.online === true
  const tone: Tone = reachable === true ? "success" : remote || ownerNode ? "warning" : "neutral"
  const label = remote
    ? remote.reachable ? "远端可达" : "远端不可达，状态未知"
    : owner === data.local_node
      ? "本机 owner"
      : ownerNode?.online === true
        ? "owner 可达"
        : ownerNode?.online === false
          ? "owner 不可达，状态未知"
          : owner
            ? "owner 状态未知"
            : "owner 未知"
  return <Card size="sm">
    <CardContent className="grid gap-3">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        {reachable === false ? <Unplug aria-hidden="true" className="size-4 shrink-0 text-warning" /> : owner === data.local_node ? <Laptop aria-hidden="true" className="size-4 shrink-0 text-success" /> : <Server aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />}
        <span className="min-w-0 flex-1 font-medium">调度 owner</span>
        <StatusPill tone={tone}>{label}</StatusPill>
      </div>
      <p className="text-meta text-muted-foreground">
        {remoteOwner ? <><span className="font-medium text-foreground">{nodeName(remoteOwner, { nodes })}</span>{remote ? " · 远端会话" : owner === data.local_node ? " · 本机会话" : " · 最近已知 owner"}</> : "当前没有可确认的 owner。"}
      </p>
      {remote && !remote.reachable ? <p role="status" className="flex items-start gap-2 text-meta text-warning"><Unplug aria-hidden="true" className="mt-0.5 size-4 shrink-0" />远端不可达时，排队和回复结果不能视为已执行。</p> : null}
    </CardContent>
  </Card>
}

function JobRow({ job, data, botId, nodes }: { job: ManagerJob; data: ConversationPayload; botId?: string; nodes: ReturnType<typeof useWorkspace>["nodes"] }) {
  const kind = triggerKind(job.trigger)
  const targetBotId = botId
  return <li className="grid min-w-0 gap-2 rounded-xl border bg-card p-3 shadow-xs">
    <div className="flex min-w-0 flex-wrap items-center gap-2">
      <span className="grid size-7 shrink-0 place-items-center rounded-lg bg-muted text-muted-foreground"><Clock3 aria-hidden="true" className="size-4" /></span>
      <span className="min-w-0 flex-1">
        <span className="block truncate font-medium" title={job.trigger}>{kind.label}</span>
        <span className="block truncate text-meta text-muted-foreground">{kind.detail}</span>
      </span>
      <StatusPill tone={schedulerTone(job.state)}>{schedulerLabel(job.state)}</StatusPill>
    </div>
    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-meta text-muted-foreground">
      <span>{job.source_node ? `来自 ${nodeName(job.source_node, { nodes })}` : data.scheduler?.remote ? "远端 owner" : "本机调度"}</span>
      {job.pending_check ? <StatusPill tone="warning" icon={CircleQuestionMark}>待核对</StatusPill> : null}
      {job.failure ? <span className="min-w-0 truncate text-danger" title={job.failure.detail}>{job.failure.detail}</span> : null}
    </div>
    {targetBotId ? <Link to="/b/$botId" params={{ botId: targetBotId }} className="hit-area relative w-fit text-meta font-medium text-foreground underline-offset-4 hover:underline">打开对话</Link> : null}
    <Details>
      <DetailRow label="调度状态" value={job.state} mono />
      <DetailRow label="触发器" value={job.trigger} mono />
      {job.message_id ? <DetailRow label="消息编号" value={job.message_id} mono /> : null}
      {job.reply_message_id ? <DetailRow label="回复编号" value={job.reply_message_id} mono /> : null}
      {job.outcome ? <DetailRow label="补充结果" value={job.outcome} /> : null}
    </Details>
  </li>
}

function WorkRow({ work, botId, nodes }: { work: WorkRecord; botId?: string; nodes: ReturnType<typeof useWorkspace>["nodes"] }) {
  const state = shownState(work, nodes)
  return <li className="grid min-w-0 gap-2 rounded-xl border bg-card p-3 shadow-xs">
    <div className="flex min-w-0 flex-wrap items-center gap-2">
      <span className="grid size-7 shrink-0 place-items-center rounded-lg bg-info-soft text-info"><CircleStop aria-hidden="true" className="size-4" /></span>
      <span className="min-w-0 flex-1">
        <span className="block min-w-0 truncate font-medium" title={work.title || work.prompt}>{work.title || "执行工作"}</span>
        <span className="block text-meta text-muted-foreground">dispatch_work · {nodeName(work.target_node, { nodes })}</span>
      </span>
      <StatusPill tone={state === "failed" || state === "rejected" ? "danger" : state === "awaiting_confirmation" || state === "unverified" ? "warning" : state === "completed" ? "success" : "info"}>{taskLabel(state)}</StatusPill>
    </div>
    {work.summary ? <p className="line-clamp-3 whitespace-pre-wrap break-words text-meta text-muted-foreground">{work.summary}</p> : null}
    <div className="flex flex-wrap items-center gap-2">
      {botId ? <Link to="/b/$botId/work/$workId" params={{ botId, workId: work.work_id }} className="hit-area relative w-fit text-meta font-medium text-foreground underline-offset-4 hover:underline">打开任务详情</Link> : null}
      <Details>
        <DetailRow label="任务编号" value={work.work_id} mono />
        <DetailRow label="任务状态" value={work.state} mono />
        {work.outcome ? <DetailRow label="结果" value={work.outcome} /> : null}
        {work.stop ? <DetailRow label="停止请求" value={work.stop} /> : null}
      </Details>
    </div>
  </li>
}

function Unanswered({ ids, data, botId }: { ids: string[]; data: ConversationPayload; botId?: string }) {
  if (!ids.length) return null
  return <Section title="未等到回复" count={ids.length}>
    <Card size="sm"><CardContent className="grid gap-1">
      {ids.map((id) => {
        const message = data.messages.find((item) => item.message_id === id)
        const text = message?.text || `消息 ${id}`
        return botId ? <Link key={id} to="/b/$botId" params={{ botId }} className="hit-area relative grid min-w-0 gap-0.5 rounded-lg px-2 py-2 text-start hover:bg-muted focus-visible:ring-3 focus-visible:ring-ring/50">
          <span className="truncate font-medium">消息未等到回复</span><span className="line-clamp-2 text-meta text-muted-foreground">{text}</span>
        </Link> : <div key={id} className="grid min-w-0 gap-0.5 px-2 py-2"><span className="font-medium">消息未等到回复</span><span className="line-clamp-2 text-meta text-muted-foreground">{text}</span></div>
      })}
    </CardContent></Card>
  </Section>
}

function QuestionRows({ questions, botId }: { questions: Question[]; botId?: string }) {
  if (!questions.length) return null
  return <Section title="问题与确认" count={questions.length}>
    <Card size="sm"><CardContent className="grid gap-1">
      {questions.map((question) => {
        const open = questionOpen(question)
        const target = question.bot?.bot_id ?? botId
        const status = open ? "待回答" : question.answer ? "已回答" : "已失效"
        const tone: Tone = open ? "warning" : question.answer ? "success" : "neutral"
        const body = <><span className="flex min-w-0 items-center gap-2"><CircleQuestionMark aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" /><span className="min-w-0 flex-1 truncate font-medium" title={question.text}>{question.text}</span><StatusPill tone={tone}>{status}</StatusPill></span><span className="ps-6 text-meta text-muted-foreground">{question.bot?.name ?? "当前 Bot"} · {when(question.created_at_ms)}</span></>
        return target ? <Link key={question.question_id} to="/b/$botId" params={{ botId: target }} className="hit-area relative grid min-w-0 gap-1 rounded-lg px-2 py-2 text-start hover:bg-muted focus-visible:ring-3 focus-visible:ring-ring/50">{body}</Link> : <div key={question.question_id} className="grid min-w-0 gap-1 px-2 py-2">{body}</div>
      })}
    </CardContent></Card>
  </Section>
}

function SessionRows({ sessions, nodes }: { sessions: SessionHandoff[]; nodes: ReturnType<typeof useWorkspace>["nodes"] }) {
  if (!sessions.length) return null
  return <Section title="Provider 会话交接" count={sessions.length}>
    <Card size="sm"><CardContent className="grid gap-3">
      {sessions.map((session) => {
        const Icon = providerIcons[session.agent]
        return <div key={`${session.node}:${session.session_id}`} className={cn("grid min-w-0 gap-1 rounded-lg border border-dashed p-3", session.muted && "text-muted-foreground")}>
          <div className="flex min-w-0 items-center gap-2"><span className="grid size-7 shrink-0 place-items-center rounded-lg bg-muted"><Icon aria-hidden="true" className="size-4" /></span><span className="min-w-0 flex-1 truncate font-medium">{session.muted ? "已停止关注" : "接手会话"} · {providerNames[session.agent]}</span><StatusPill tone={session.muted ? "neutral" : "info"}>{session.muted ? "已静音" : "关注中"}</StatusPill></div>
          <p className="ps-9 text-meta text-muted-foreground">{nodeName(session.node, { nodes })} · {session.cwd || "未记录项目"}</p>
          {session.summary ? <p className="ps-9 line-clamp-3 whitespace-pre-wrap break-words text-meta text-muted-foreground">{session.summary}</p> : null}
          <time className="ps-9 text-meta text-muted-foreground" dateTime={new Date(session.created_at_ms).toISOString()}>{when(session.created_at_ms)}</time>
        </div>
      })}
    </CardContent></Card>
  </Section>
}

function SentMessage({ sent, botName, botId }: { sent: SentBotMessage; botName: string; botId?: string }) {
  const body = <Card size="sm"><CardContent className="grid gap-2">
    <div className="flex min-w-0 flex-wrap items-center gap-2"><Send aria-hidden="true" className="size-4 shrink-0 text-info" /><span className="min-w-0 flex-1 truncate font-medium">发给 {botName}</span><StatusPill tone="info">message_bot · 协作消息</StatusPill></div>
    <p className="line-clamp-4 whitespace-pre-wrap break-words text-meta">{sent.text}</p>
    <time className="text-meta text-muted-foreground" dateTime={new Date(sent.created_at_ms).toISOString()}>{when(sent.created_at_ms)}</time>
  </CardContent></Card>
  return botId ? <Link to="/b/$botId" params={{ botId }} className="block rounded-xl outline-none focus-visible:ring-3 focus-visible:ring-ring/50">{body}</Link> : body
}

function CollaborationMessages({ data, botNames }: { data: ConversationPayload; botNames: Map<string, { bot_id: string; name: string }> }) {
  const incoming = data.messages.filter((message) => message.role === "bot" && message.sender)
  const sent = data.sent
  if (!incoming.length && !sent.length) return null
  return <Section title="Bot 协作消息" count={incoming.length + sent.length}>
    <div className="grid gap-3">
      {incoming.map((message) => {
        const sender = message.sender!
        const senderBot = botNames.get(sender.conversation_id)
        return <div key={`received:${message.message_id}`} className="grid min-w-0 gap-2 rounded-xl border bg-card p-3 shadow-xs">
          <div className="flex min-w-0 flex-wrap items-center gap-2"><Bot aria-hidden="true" className="size-4 shrink-0 text-info" />{senderBot ? <Link to="/b/$botId" params={{ botId: senderBot.bot_id }} className="hit-area relative min-w-0 truncate font-medium underline-offset-4 hover:underline">{sender.name}</Link> : <span className="truncate font-medium">{sender.name}</span>}<StatusPill tone="info">message_bot · 协作消息</StatusPill></div>
          <Message message={message} />
        </div>
      })}
      {sent.map((message) => {
        const target = botNames.get(message.to)
        return <SentMessage key={`sent:${message.message_id}`} sent={message} botName={target?.name ?? "另一个 Bot"} botId={target?.bot_id} />
      })}
    </div>
    <p className="px-1 text-meta text-muted-foreground">协作消息不会自动变成执行工作。执行工作只来自 dispatch_work，并在上方任务详情中单独展示。</p>
  </Section>
}

export function CollaborationPanel({ data: supplied, botId: suppliedBotId, className }: CollaborationPanelProps = {}) {
  const workspace = useWorkspace()
  const data = supplied === undefined ? workspace.data : supplied
  const botId = suppliedBotId ?? workspace.selectedBot?.bot_id
  const botNames = new Map(workspace.bots.map((bot) => [bot.conversation_id, { bot_id: bot.bot_id, name: bot.name }]))
  if (!data) return <Card className={cn("min-w-0", className)}><CardContent className="py-6 text-meta text-muted-foreground">连接后才能查看协作状态。</CardContent></Card>
  const scheduler = data.scheduler
  const jobs = scheduler?.jobs ?? []
  const works = data.works
  const questions = data.questions
  return <div className={cn("grid min-w-0 gap-5", className)}>
    <OwnerCard data={data} nodes={workspace.nodes} />
    {scheduler ? <Section title="本机会话队列" count={jobs.length}>
      {jobs.length ? <ul className="grid gap-2">{jobs.map((job) => <JobRow key={job.trigger} job={job} data={data} botId={botId} nodes={workspace.nodes} />)}</ul> : <Card size="sm"><CardContent className="py-5 text-meta text-muted-foreground">当前会话没有调度项。</CardContent></Card>}
    </Section> : <Card size="sm"><CardContent className="flex items-start gap-2 py-4 text-meta text-muted-foreground"><Unplug aria-hidden="true" className="mt-0.5 size-4 shrink-0" />调度状态暂不可见。</CardContent></Card>}
    {works.length ? <Section title="执行工作" count={works.length}><ul className="grid gap-2">{works.map((work) => <WorkRow key={work.work_id} work={work} botId={botId} nodes={workspace.nodes} />)}</ul></Section> : null}
    <Unanswered ids={scheduler?.unanswered ?? []} data={data} botId={botId} />
    <QuestionRows questions={questions} botId={botId} />
    <SessionRows sessions={data.sessions} nodes={workspace.nodes} />
    <CollaborationMessages data={data} botNames={botNames} />
  </div>
}
