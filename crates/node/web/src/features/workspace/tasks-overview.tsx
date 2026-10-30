"use client"

import type { ReactNode } from "react"
import { Link } from "@tanstack/react-router"
import { ArrowUpRight, Bot, ChevronRight, CircleQuestionMark, FolderOpen, Laptop, ListChecks, LoaderCircle, Server, Wifi, WifiOff } from "lucide-react"
import { Card, CardContent } from "@/components/ui/card"
import { Button } from "@/components/ui/button"
import { CountBadge, Skeleton, StatusPill, type Tone } from "@/components/app/kit"
import { cn } from "@/lib/utils"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import type { MembershipView } from "@vibeface/client/lib/node.ts"
import { openWorksFromOverview, overviewFromWorkspace } from "@vibeface/client/workspace/model.ts"
import { nodeName, shownState, staleTaskLabel, taskActive, taskLabel, taskStale } from "@vibeface/client/workspace/labels.ts"
import { projectName } from "@vibeface/client/workspace/questions.ts"
import type { NodeInfo, OpenWork, OverviewTask } from "@vibeface/client/workspace/types.ts"
import { useInbox } from "./inbox"
import { ConfirmCard } from "./task-activity"
import { QuestionCard } from "./question-card"
import { useCreateBot } from "./workspace-shell"

type TaskGroup = "working" | "waiting"
type ProjectedWork = Extract<OverviewTask, { kind: "work" | "dispatch" }>

const stateTone = (state: string): Tone => state === "failed" || state === "dispatch_rejected" ? "danger" : state === "completed" ? "success" : ["waiting", "awaiting_confirmation", "unverified", "unknown", "accepted_waiting_sync", "dispatch_unverified"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"

/** The personal task surface. It only derives display rows from the already parsed workspace context. */
export function TasksOverview() {
  const workspace = useWorkspace()
  const createBot = useCreateBot()
  const { directory, ready } = useDevice()
  const inbox = useInbox()
  type InboxItem = (typeof inbox)[number]
  const lookup = { nodes: workspace.nodes, directory }
  const localOverview = overviewFromWorkspace({ openWorks: workspace.openWorks, bots: workspace.bots, questions: workspace.questions, nodes: workspace.nodes })
  const overview = workspace.overview ?? localOverview
  const overviewWorks = workspace.overview ? openWorksFromOverview(workspace.overview) : workspace.openWorks
  const attention = inbox.filter((item): item is Extract<InboxItem, { kind: "question" | "confirm" }> => item.kind === "confirm" || item.kind === "question" && !item.done)
  const attentionWorkIds = new Set<string>()
  for (const item of attention) {
    if (item.kind !== "confirm") continue
    if (item.check.kind === "work") attentionWorkIds.add(item.check.work_id)
    if (item.check.kind === "device") for (const work of item.check.works) attentionWorkIds.add(work.work_id)
  }
  const openQuestions = attention.filter((item): item is Extract<InboxItem, { kind: "question" }> => item.kind === "question").map((item) => item.question)
  const waitingSessions = new Set(openQuestions.filter((question) => question.about.request_id).map((question) => question.about.session_id))
  const projected = new Map(overview.tasks.filter((task): task is ProjectedWork => task.kind === "work" || task.kind === "dispatch").filter((task) => task.work_id).map((task) => [task.work_id!, task]))
  const tasks = overviewWorks.filter((work) => projected.has(work.work_id) && !attentionWorkIds.has(work.work_id))
  const groups: Record<TaskGroup, OpenWork[]> = { working: [], waiting: [] }
  for (const work of tasks) groups[taskGroup(work, workspace.nodes, waitingSessions, projected.get(work.work_id))].push(work)
  const hasContent = attention.length > 0 || groups.working.length > 0 || groups.waiting.length > 0
  const bot = workspace.selectedBot ?? workspace.bots[0]

  if (workspace.incomplete && !workspace.nodes.length) return <div className="mx-auto grid w-full max-w-(--page-width) gap-3" aria-busy="true"><Skeleton className="h-20" /><Skeleton className="h-20" /><Skeleton className="h-20" /></div>
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <p className="sr-only" role="status" aria-live="polite">{hasContent ? `${attention.length + tasks.length} 项未完成任务` : "没有未完成任务"}</p>
    <WorkspaceMasthead attention={attention.length} working={groups.working.length} waiting={groups.waiting.length} bots={workspace.bots.length} online={workspace.connectionOnline} />
    {attention.length ? <OverviewSection title="需要留意" count={attention.length}>
      {attention.map((item) => <div key={item.key}>{item.kind === "question" ? <QuestionCard question={item.question} inbox from="tasks" /> : <ConfirmCard check={item.check} inbox from="tasks" />}</div>)}
    </OverviewSection> : null}
    {groups.working.length ? <OverviewSection title="进行中" count={groups.working.length}>
      {groups.working.map((work) => <TaskOverviewCard key={work.work_id} work={work} projected={projected.get(work.work_id)} detailAvailable={workspace.openWorks.some((item) => item.work_id === work.work_id)} lookup={lookup} readyDevice={ready?.entry.device_id} connected={workspace.connectionOnline} />)}
    </OverviewSection> : null}
    {groups.waiting.length ? <OverviewSection title="等待中" count={groups.waiting.length}>
      {groups.waiting.map((work) => <TaskOverviewCard key={work.work_id} work={work} projected={projected.get(work.work_id)} detailAvailable={workspace.openWorks.some((item) => item.work_id === work.work_id)} lookup={lookup} readyDevice={ready?.entry.device_id} connected={workspace.connectionOnline} />)}
    </OverviewSection> : null}
    {!hasContent ? <WorkspaceEmpty bot={bot} onCreate={createBot} /> : null}
  </div>
}

function WorkspaceMasthead({ attention, working, waiting, bots, online }: { attention: number; working: number; waiting: number; bots: number; online: boolean }) {
  const lead = attention ? `${attention} 项需要你先处理，其他工作会在这里继续推进。` : working ? `${working} 项正在推进，等待中的工作会单独留在视线里。` : bots ? "从这里开始查看每个 Bot 的执行节奏和下一步。" : "先创建一个 Bot，任务总览会从第一项工作开始生长。"
  return <section className="relative overflow-hidden rounded-2xl border border-brand/15 bg-linear-to-br from-brand-soft via-card to-card p-5 shadow-sm sm:p-6">
    <div aria-hidden="true" className="pointer-events-none absolute -end-16 -top-20 size-56 rounded-full bg-brand/10 blur-3xl" />
    <div className="relative grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(18rem,auto)] lg:items-end">
      <div className="grid min-w-0 gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-meta font-semibold uppercase tracking-[0.16em] text-brand">个人 Agent 工作台</span>
          <StatusPill tone={online ? "success" : "warning"} icon={online ? Wifi : WifiOff}>{online ? "网络在线" : "连接待恢复"}</StatusPill>
        </div>
        <div className="grid gap-1.5">
          <h2 className="text-2xl font-semibold tracking-tight text-balance sm:text-[1.75rem]">掌握每个 Bot 的下一步</h2>
          <p className="max-w-[48ch] text-meta leading-relaxed text-muted-foreground">{lead} 把执行、等待和协作放在一个节奏里。</p>
        </div>
      </div>
      <div className="grid grid-cols-3 gap-2" aria-label="任务概览">
        <Metric label="需要留意" value={attention} tone={attention ? "warning" : "neutral"} />
        <Metric label="进行中" value={working} tone={working ? "info" : "neutral"} />
        <Metric label="等待中" value={waiting} tone={waiting ? "neutral" : "neutral"} />
      </div>
    </div>
  </section>
}

function Metric({ label, value, tone }: { label: string; value: number; tone: Tone }) {
  return <div className="grid min-w-0 gap-1 rounded-xl border border-border/70 bg-background/55 px-3 py-2.5 backdrop-blur-sm">
    <span className="truncate text-meta text-muted-foreground">{label}</span>
    <span className={tone === "warning" ? "text-xl font-semibold tabular-nums text-warning" : tone === "info" ? "text-xl font-semibold tabular-nums text-info" : "text-xl font-semibold tabular-nums text-foreground"}>{value}</span>
  </div>
}

function WorkspaceEmpty({ bot, onCreate }: { bot?: { bot_id: string; name: string }; onCreate: () => void }) {
  return <section className="relative overflow-hidden rounded-2xl border border-dashed border-brand/25 bg-linear-to-b from-brand-soft/45 to-card px-6 py-14 text-center shadow-sm sm:py-16">
    <div aria-hidden="true" className="pointer-events-none absolute inset-x-1/4 -top-24 h-44 rounded-full bg-brand/10 blur-3xl" />
    <div className="relative mx-auto flex max-w-md flex-col items-center gap-3">
      <span className="grid size-16 place-items-center rounded-2xl bg-brand-soft text-brand ring-1 ring-brand/20"><ListChecks aria-hidden="true" className="size-8" /></span>
      <span className="text-meta font-semibold uppercase tracking-[0.16em] text-brand">随时开始</span>
      <h2 className="text-heading font-semibold">还没有需要盯住的任务</h2>
      <p className="max-w-sm text-meta leading-relaxed text-muted-foreground">从 Bot 对话里发出第一条请求后，执行步骤、等待确认和协作消息会在这里汇合。</p>
      {bot ? <Button asChild className="mt-2 gap-2"><Link to="/b/$botId" params={{ botId: bot.bot_id }}>打开 {bot.name}<ArrowUpRight aria-hidden="true" className="size-4" /></Link></Button> : <Button type="button" className="mt-2 gap-2" onClick={onCreate}>创建第一个 Bot<ArrowUpRight aria-hidden="true" className="size-4" /></Button>}
    </div>
  </section>
}

function taskGroup(work: OpenWork, nodes: NodeInfo[], waitingSessions: ReadonlySet<string>, projected?: ProjectedWork): TaskGroup {
  const state = projected?.state ?? shownState(work, nodes)
  return state === "unverified" || state === "awaiting_confirmation" || waitingSessions.has(work.thread_id ?? "") || !taskActive(state) ? "waiting" : "working"
}

function OverviewSection({ title, count, children }: { title: string; count: number; children: ReactNode }) {
  return <section className="grid gap-3" aria-labelledby={`tasks-${title}`}>
    <div className="flex items-center gap-2 px-1"><span aria-hidden="true" className="h-5 w-1 rounded-full bg-brand" /><h2 id={`tasks-${title}`} className="text-heading font-semibold">{title}</h2><CountBadge count={count} label={`${count} 项`} /></div>
    <div className="grid gap-3">{children}</div>
  </section>
}

function TaskOverviewCard({ work, projected, detailAvailable, lookup, readyDevice, connected }: { work: OpenWork; projected?: ProjectedWork; detailAvailable: boolean; lookup: { nodes: NodeInfo[]; directory: MembershipView | null }; readyDevice?: string; connected: boolean }) {
  const state = projected?.state ?? shownState(work, lookup.nodes)
  const stale = taskStale(state, connected)
  const unknown = state === "unknown" || state === "unverified" || state === "accepted_waiting_sync" || state === "dispatch_rejected" || state === "dispatch_unverified"
  const label = stale || state === "unknown" ? staleTaskLabel : taskLabel(state)
  const tone = stale ? "neutral" : stateTone(state)
  const Icon = stale || unknown ? CircleQuestionMark : taskActive(state) ? LoaderCircle : ListChecks
  const DeviceIcon = work.target_node === readyDevice ? Laptop : Server
  const device = nodeName(work.target_node, lookup, "未知设备")
  const project = work.cwd ? projectName(work.cwd) : "未指定项目"
  const iconSurface = stale || unknown ? "bg-warning-soft text-warning" : taskActive(state) ? "bg-brand-soft text-brand" : "bg-muted text-muted-foreground"
  const card = <Card className="min-w-0 border-transparent shadow-sm ring-1 ring-border/70 transition-[transform,box-shadow,background-color] duration-200 group-hover:-translate-y-0.5 group-hover:shadow-md max-md:min-h-11" data-navigable={projected?.navigation ? true : undefined}>
      <CardContent className="grid min-w-0 gap-3">
        <div className="flex min-w-0 items-start gap-3">
          <span className={cn("grid size-9 shrink-0 place-items-center rounded-lg ring-1 ring-inset ring-black/5", iconSurface)}><Icon aria-hidden="true" className={taskActive(state) && !stale ? "size-5 animate-spin motion-reduce:animate-none" : "size-5"} /></span>
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 flex-wrap items-start gap-2"><h3 className="min-w-0 flex-1 [overflow-wrap:anywhere] font-medium">{work.title || "任务"}</h3><StatusPill tone={tone}>{label}</StatusPill></div>
            <p className="mt-1 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-meta text-muted-foreground">
              <span className="inline-flex min-w-0 items-center gap-1.5"><Bot aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={projected?.bot?.name ?? "未知 Bot"}>{projected?.bot?.name ?? "未知 Bot"}</span></span>
              <span aria-hidden="true">·</span>
              <span className="inline-flex min-w-0 items-center gap-1.5"><DeviceIcon aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={device}>{device}</span></span>
              <span aria-hidden="true">·</span>
              <span className="inline-flex min-w-0 items-center gap-1.5"><FolderOpen aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={work.cwd || "未指定项目"}>{project}</span></span>
            </p>
          </div>
          <ChevronRight aria-hidden="true" className="mt-1 size-4 shrink-0 text-muted-foreground transition-transform duration-200 group-hover:translate-x-0.5" />
        </div>
        <p className="border-t border-border/70 pt-3 line-clamp-2 whitespace-pre-wrap break-words text-meta text-muted-foreground">{work.summary || "暂无进展"}</p>
      </CardContent>
    </Card>
  return detailAvailable && projected?.navigation?.work_id ? <Link to="/b/$botId/work/$workId" params={{ botId: projected.navigation.bot_id, workId: projected.navigation.work_id }} search={{ from: "tasks" }} className="group block min-w-0 rounded-xl outline-none focus-visible:ring-3 focus-visible:ring-ring/50">{card}</Link>
    : projected?.navigation?.bot_id ? <Link to="/b/$botId" params={{ botId: projected.navigation.bot_id }} className="group block min-w-0 rounded-xl outline-none focus-visible:ring-3 focus-visible:ring-ring/50">{card}</Link> : card
}
