"use client"

import { Bot, CircleQuestionMark, Clock3, Laptop, Send, Unplug } from "lucide-react"
import { Link } from "@tanstack/react-router"
import { Card, CardContent } from "@/components/ui/card"
import { Details, DetailRow, StatusPill, type Tone } from "@/components/app/kit"
import { Message } from "@/features/chat/elements/message"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { externalSessionLabels, externalSessionStatus, nodeName, schedulerStatus } from "@vibeface/client/workspace/labels.ts"
import { providerIcons } from "@/features/devices/ai-settings"
import { providerNames } from "@vibeface/client/lib/node.ts"
import type { ConversationPayload, ManagerJob, SessionHandoff, SentBotMessage } from "@vibeface/client/workspace/types.ts"
import { cn } from "@/lib/utils"

/** A read-only view of this conversation's local scheduler and Bot-to-Bot records. */
export type CollaborationPanelProps = { data: ConversationPayload; botId: string; className?: string }

function schedulerTone(state: string): Tone {
  if (state === "failed" || state === "interrupted") return "danger"
  if (state === "cancelled") return "warning"
  if (schedulerStatus(state).group === "history") return "success"
  if (schedulerStatus(state).group === "open") return "info"
  return "neutral"
}

function triggerKind(trigger: string) {
  if (trigger.startsWith("bot_message:")) return { label: "Bot 协作消息", detail: "Bot 之间的消息" }
  if (trigger.startsWith("task:")) return { label: "执行工作", detail: "执行任务" }
  if (trigger.startsWith("work:")) return { label: "工作结果", detail: "结果记录" }
  if (trigger.startsWith("schedule:")) return { label: "定时任务", detail: "定时提醒" }
  if (trigger.startsWith("message:")) return { label: "用户消息", detail: "普通消息" }
  return { label: "队列项目", detail: "未知触发" }
}

function when(ms: number) {
  return new Date(ms).toLocaleString("zh-CN", { dateStyle: "medium", timeStyle: "short" })
}

function Section({ title, count, children, className }: { title: string; count?: number; children: React.ReactNode; className?: string }) {
  return <section aria-label={title} className={cn("grid gap-2", className)}>
    <h3 className="flex items-center gap-2 px-1 text-meta font-medium text-muted-foreground"><span aria-hidden="true" className="h-4 w-1 rounded-full bg-brand" />
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
      ? "由本机负责"
      : ownerNode?.online === true
        ? "负责设备可达"
        : ownerNode?.online === false
          ? "负责设备不可达，状态未知"
          : owner
            ? "负责设备状态未知"
            : "负责设备未知"
  return <Card size="sm" className="border-transparent bg-linear-to-br from-info-soft/35 via-card to-card ring-1 ring-border/70">
    <CardContent className="grid gap-3">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        {reachable === false ? <Unplug aria-hidden="true" className="size-4 shrink-0 text-warning" /> : owner === data.local_node ? <Laptop aria-hidden="true" className="size-4 shrink-0 text-success" /> : <Laptop aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />}
        <span className="min-w-0 flex-1 font-medium">负责设备</span>
        <StatusPill tone={tone}>{label}</StatusPill>
      </div>
      <p className="text-meta text-muted-foreground">
        {remoteOwner ? <><span className="font-medium text-foreground">{nodeName(remoteOwner, { nodes })}</span>{remote ? " · 其他设备负责" : owner === data.local_node ? " · 本机负责" : " · 最近已知负责设备"}</> : "当前没有可确认的负责设备。"}
      </p>
      {remote && !remote.reachable ? <p role="status" className="flex items-start gap-2 text-meta text-warning"><Unplug aria-hidden="true" className="mt-0.5 size-4 shrink-0" />远端不可达时，排队和回复结果不能视为已执行。</p> : null}
    </CardContent>
  </Card>
}

function JobRow({ job, data, botId, nodes }: { job: ManagerJob; data: ConversationPayload; botId?: string; nodes: ReturnType<typeof useWorkspace>["nodes"] }) {
  const kind = triggerKind(job.trigger)
  const targetBotId = botId
  return <li className="grid min-w-0 gap-2 rounded-xl border bg-card p-3 shadow-xs transition-[transform,box-shadow] duration-200 hover:-translate-y-0.5 hover:shadow-sm">
    <div className="flex min-w-0 flex-wrap items-center gap-2">
      <span className="grid size-7 shrink-0 place-items-center rounded-lg bg-muted text-muted-foreground"><Clock3 aria-hidden="true" className="size-4" /></span>
      <span className="min-w-0 flex-1">
        <span className="block truncate font-medium" title={job.trigger}>{kind.label}</span>
        <span className="block truncate text-meta text-muted-foreground">{kind.detail}</span>
      </span>
      <StatusPill tone={schedulerTone(job.state)}>{schedulerStatus(data.scheduler?.remote?.reachable === false && schedulerStatus(job.state).group !== "history" ? "unknown" : job.state).label}</StatusPill>
    </div>
    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-meta text-muted-foreground">
      <span>{job.source_node ? `来自 ${nodeName(job.source_node, { nodes })}` : data.scheduler?.remote ? "其他设备负责" : "本机记录"}</span>
      {job.pending_check ? <StatusPill tone="warning" icon={CircleQuestionMark}>需要确认</StatusPill> : null}
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

function SessionRows({ sessions, nodes, connected }: { sessions: SessionHandoff[]; nodes: ReturnType<typeof useWorkspace>["nodes"]; connected: boolean }) {
  return <Section title="Bot 关注的外部会话" count={sessions.length}>
    <Card size="sm"><CardContent className="grid gap-3">
      {sessions.map((session) => {
        const Icon = providerIcons[session.agent]
        const reach = connected ? externalSessionStatus(session, nodes) : "unknown"
        return <div key={`${session.node}:${session.session_id}`} className={cn("grid min-w-0 gap-1 rounded-lg border border-dashed p-3", session.muted && "text-muted-foreground")}>
          <div className="flex min-w-0 items-center gap-2"><span className="grid size-7 shrink-0 place-items-center rounded-lg bg-muted"><Icon aria-hidden="true" className="size-4" /></span><span className="min-w-0 flex-1 truncate font-medium">{providerNames[session.agent]}</span><StatusPill tone={session.muted ? "neutral" : "info"}>{session.muted ? "已静音" : "关注中"}</StatusPill><StatusPill tone={reach === "revoked" ? "danger" : reach === "offline" ? "warning" : "neutral"}>{externalSessionLabels[reach]}</StatusPill></div>
          <p className="ps-9 text-meta text-muted-foreground">{nodeName(session.node, { nodes })} · {session.cwd || "未记录项目"}</p>
          {session.summary ? <p className="ps-9 line-clamp-3 whitespace-pre-wrap break-words text-meta text-muted-foreground">{session.summary}</p> : null}
          <time className="ps-9 text-meta text-muted-foreground" dateTime={new Date(session.created_at_ms).toISOString()}>{when(session.created_at_ms)}</time>
          <Details label="查看归属记录详情" className="ps-9"><DetailRow label="外部会话编号" value={session.session_id} mono /><p>这是外部会话归属记录。设备可达不代表会话仍在运行，也不能证明 Bot 自身原生会话可续接。</p></Details>
        </div>
      })}
      {!sessions.length ? <p className="text-meta text-muted-foreground">尚无外部会话归属记录。Bot 自身运行不会自动建立这类外部记录。</p> : null}
    </CardContent></Card>
  </Section>
}

function SentMessage({ sent, botName, botId }: { sent: SentBotMessage; botName: string; botId?: string }) {
  const body = <Card size="sm"><CardContent className="grid gap-2">
    <div className="flex min-w-0 flex-wrap items-center gap-2"><Send aria-hidden="true" className="size-4 shrink-0 text-info" /><span className="min-w-0 flex-1 truncate font-medium">发给 {botName}</span><StatusPill tone="info">协作消息</StatusPill></div>
    <p className="line-clamp-4 whitespace-pre-wrap break-words text-meta">{sent.text}</p>
    <time className="text-meta text-muted-foreground" dateTime={new Date(sent.created_at_ms).toISOString()}>{when(sent.created_at_ms)}</time>
    <p className="text-meta text-muted-foreground">这里只记录了发出的消息，对方是否收到或回复尚不清楚。</p>
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
    <p className="px-1 text-meta text-muted-foreground">协作消息与任务分别记录，执行情况在任务详情中查看。</p>
  </Section>
}

export function CollaborationPanel({ data, botId, className }: CollaborationPanelProps) {
  const workspace = useWorkspace()
  const botNames = new Map(workspace.bots.map((bot) => [bot.conversation_id, { bot_id: bot.bot_id, name: bot.name }]))
  const scheduler = data.scheduler
  const jobs = scheduler?.jobs ?? []
  const open = jobs.filter((job) => schedulerStatus(job.state).group !== "history")
  const history = jobs.filter((job) => schedulerStatus(job.state).group === "history")
  return <div className={cn("grid min-w-0 gap-5", className)}>
    <OwnerCard data={data} nodes={workspace.nodes} />
    {scheduler ? <Section title="未结束的后台工作" count={open.length}>
      <p className="px-1 text-meta text-muted-foreground">显示最近状态，不代表实际执行顺序。其他设备可能只显示近期部分记录。</p>
      {open.length ? <ul className="grid gap-2">{open.map((job) => <JobRow key={job.trigger} job={job} data={data} botId={botId} nodes={workspace.nodes} />)}</ul> : <Card size="sm"><CardContent className="py-5 text-meta text-muted-foreground">{scheduler.remote?.reachable === false ? "负责设备不可达，当前后台工作状态未知。" : "当前记录没有未结束的后台工作。"}</CardContent></Card>}
      {history.length ? <Details label={`后台工作历史 · ${history.length}`}><ul className="grid gap-2">{history.map((job) => <JobRow key={job.trigger} job={job} data={data} botId={botId} nodes={workspace.nodes} />)}</ul></Details> : null}
    </Section> : <Card size="sm"><CardContent className="flex items-start gap-2 py-4 text-meta text-muted-foreground"><Unplug aria-hidden="true" className="mt-0.5 size-4 shrink-0" />后台工作状态暂未提供。</CardContent></Card>}
    <Unanswered ids={scheduler?.unanswered ?? []} data={data} botId={botId} />
    <SessionRows sessions={data.sessions} nodes={workspace.nodes} connected={workspace.connectionOnline && workspace.synced} />
    <CollaborationMessages data={data} botNames={botNames} />
  </div>
}
