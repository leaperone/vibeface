"use client"

import type { ReactNode } from "react"
import { useState } from "react"
import { Link } from "@tanstack/react-router"
import { Bot, ChevronRight, CircleQuestionMark, CircleX, FolderOpen, Laptop, ListChecks, LoaderCircle, MessageCircle, Plus, Wifi, WifiOff } from "lucide-react"
import { Card, CardContent } from "@/components/ui/card"
import { Button } from "@/components/ui/button"
import { CountBadge, EmptyState, Segmented, Skeleton, StatusPill, type Tone } from "@/components/app/kit"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { cn } from "@/lib/utils"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import type { MembershipView } from "@vibeface/client/lib/node.ts"
import { openWorksFromOverview, overviewFromWorkspace } from "@vibeface/client/workspace/model.ts"
import { cardState, needsLook, nodeName, staleTaskLabel, taskActive, taskGroup, taskGroups, taskLabel, taskProgress, taskStale, type TaskGroup } from "@vibeface/client/workspace/labels.ts"
import { projectName } from "@vibeface/client/workspace/questions.ts"
import { workFailure } from "@vibeface/client/lib/errors.ts"
import type { NodeInfo, OpenWorkCard, OverviewTask } from "@vibeface/client/workspace/types.ts"
import { useInbox } from "./inbox"
import { ConfirmCard } from "./task-activity"
import { QuestionCard } from "./question-card"
import { useCreateBot } from "./workspace-shell"

type ProjectedWork = Extract<OverviewTask, { kind: "work" | "dispatch" }>

const stateTone = (state: string): Tone => state === "failed" || state === "dispatch_rejected" ? "danger" : state === "completed" ? "success" : ["waiting", "awaiting_confirmation", "unverified", "unknown", "accepted_waiting_sync", "dispatch_unverified"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"

/** The personal task surface. It only derives display rows from the already parsed workspace context. */
export function TasksOverview() {
  const workspace = useWorkspace()
  const createBot = useCreateBot()
  const { directory } = useDevice()
  const [filter, setFilter] = useState<TaskGroup | "all">("all")
  const [botFilter, setBotFilter] = useState("all")
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
  const groups: Record<TaskGroup, OpenWorkCard[]> = { attention: [], working: [], waiting: [] }
  const botIds = new Map(workspace.bots.map((bot) => [bot.conversation_id, bot.bot_id]))
  const matchesBot = (conversationId: string, projectedBotId?: string | null) => botFilter === "all" || projectedBotId === botFilter || botIds.get(conversationId) === botFilter
  for (const work of tasks) {
    const item = projected.get(work.work_id)
    if (matchesBot(work.conversation_id, item?.bot?.bot_id)) groups[taskGroup(cardState(work, workspace.nodes, item?.state), waitingSessions.has(work.thread_id ?? ""))].push(work)
  }
  const visibleAttention = attention.flatMap((item): typeof attention => {
    if (item.kind === "question") return matchesBot(item.question.conversation_id, item.question.bot?.bot_id) ? [item] : []
    const check = item.check
    if (check.kind === "device") {
      const works = check.works.filter((work) => matchesBot(work.conversation_id))
      return works.length ? [{ ...item, check: { ...check, works } }] : []
    }
    return matchesBot(check.kind === "work" ? check.conversation_id : workspace.selectedBot?.conversation_id ?? "") ? [item] : []
  })
  const visible = (group: TaskGroup) => filter === "all" || filter === group
  const noticed = visible("attention") ? visibleAttention.length + groups.attention.length : 0
  const visibleCount = noticed + (visible("working") ? groups.working.length : 0) + (visible("waiting") ? groups.waiting.length : 0)
  const hasContent = visibleCount > 0
  const emptyLabel = filter === "all" && botFilter === "all" ? "没有未完成的任务" : "没有符合筛选的任务"
  const card = (work: OpenWorkCard) => <TaskOverviewCard key={work.work_id} work={work} projected={projected.get(work.work_id)} lookup={lookup} connected={workspace.connectionOnline} progress={taskProgress(work, workspace.liveProgress)} />
  const bot = workspace.bots.find((item) => item.bot_id === botFilter) ?? workspace.selectedBot ?? workspace.bots[0]

  if (workspace.incomplete && !workspace.nodes.length) return <div className="mx-auto grid w-full max-w-(--page-width) gap-3" aria-busy="true"><Skeleton className="h-20" /><Skeleton className="h-20" /><Skeleton className="h-20" /></div>
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <p className="sr-only" role="status" aria-live="polite">{hasContent ? `${visibleCount} 项任务` : emptyLabel}</p>
    <WorkspaceMasthead attention={visibleAttention.length + groups.attention.length} working={groups.working.length} waiting={groups.waiting.length} online={workspace.connectionOnline} />
    <TaskFilters filter={filter} botFilter={botFilter} bots={workspace.bots} onFilter={setFilter} onBotFilter={setBotFilter} />
    {taskGroups.map(({ group, title }) => {
      if (!visible(group)) return null
      const count = group === "attention" ? noticed : groups[group].length
      return count ? <OverviewSection key={group} title={title} count={count}>
        {group === "attention" ? visibleAttention.map((item) => <div key={item.key}>{item.kind === "question" ? <QuestionCard question={item.question} inbox from="tasks" /> : <ConfirmCard check={item.check} inbox from="tasks" />}</div>) : null}
        {groups[group].map(card)}
      </OverviewSection> : null
    })}
    {!hasContent ? <WorkspaceEmpty title={emptyLabel} bot={bot} onCreate={createBot} /> : null}
  </div>
}

function WorkspaceMasthead({ attention, working, waiting, online }: { attention: number; working: number; waiting: number; online: boolean }) {
  const lead = attention ? `${attention} 项需要你先处理，其他工作会继续留在这里。` : working ? `${working} 项正在推进，等待中的工作会单独留在视线里。` : "从每个 Bot 的下一步开始。"
  return <section className="relative overflow-hidden rounded-2xl border border-brand/15 bg-linear-to-br from-brand-soft/55 via-card to-card p-5 shadow-sm sm:p-6" aria-label="个人任务总览">
    <div aria-hidden="true" className="pointer-events-none absolute -end-16 -top-20 size-56 rounded-full bg-brand/10 blur-3xl" />
    <div className="relative grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(18rem,auto)] lg:items-end">
      <div className="grid min-w-0 gap-3">
        <div className="flex flex-wrap items-center gap-2"><span className="text-meta font-semibold uppercase tracking-[0.16em] text-brand">个人任务总览</span><StatusPill tone={online ? "success" : "warning"} icon={online ? Wifi : WifiOff}>{online ? "网络在线" : "连接待恢复"}</StatusPill></div>
        <div className="grid gap-1.5"><h2 className="text-2xl font-semibold tracking-tight text-balance sm:text-[1.75rem]">掌握每个 Bot 的下一步</h2><p className="max-w-[48ch] text-meta leading-relaxed text-muted-foreground">{lead}</p></div>
      </div>
      <div className="grid grid-cols-3 gap-2" aria-label="任务概览"><Metric label="需要留意" value={attention} tone={attention ? "warning" : "neutral"} /><Metric label="进行中" value={working} tone={working ? "info" : "neutral"} /><Metric label="等待中" value={waiting} tone="neutral" /></div>
    </div>
  </section>
}

function Metric({ label, value, tone }: { label: string; value: number; tone: Tone }) {
  return <div className="grid min-w-0 gap-1 rounded-xl border border-border/70 bg-background/55 px-3 py-2.5"><span className="truncate text-meta text-muted-foreground">{label}</span><span className={cn("text-xl font-semibold tabular-nums", tone === "warning" ? "text-warning" : tone === "info" ? "text-info" : "text-foreground")}>{value}</span></div>
}

function TaskFilters({ filter, botFilter, bots, onFilter, onBotFilter }: { filter: TaskGroup | "all"; botFilter: string; bots: readonly { bot_id: string; name: string }[]; onFilter: (value: TaskGroup | "all") => void; onBotFilter: (value: string) => void }) {
  const options = [{ value: "all" as const, label: "全部" }, ...taskGroups.map(({ group, title }) => ({ value: group, label: title }))]
  return <section className="flex min-w-0 flex-wrap items-center justify-between gap-3 rounded-xl border bg-card/70 px-3 py-2.5 shadow-xs" aria-label="任务筛选">
    <Segmented label="任务状态" value={filter} options={options} onChange={onFilter} />
    <NativeSelect size="sm" aria-label="按 Bot 筛选" value={botFilter} onChange={(event) => onBotFilter(event.currentTarget.value)}>
      <NativeSelectOption value="all">所有 Bot</NativeSelectOption>
      {bots.map((bot) => <NativeSelectOption key={bot.bot_id} value={bot.bot_id}>{bot.name}</NativeSelectOption>)}
    </NativeSelect>
  </section>
}

function WorkspaceEmpty({ title, bot, onCreate }: { title: string; bot?: { bot_id: string; name: string }; onCreate: () => void }) {
  return <EmptyState icon={ListChecks} title={title} action={bot ? <Button asChild variant="outline"><Link to="/b/$botId" params={{ botId: bot.bot_id }}><MessageCircle aria-hidden="true" />打开 {bot.name}</Link></Button> : <Button type="button" onClick={onCreate}><Plus aria-hidden="true" />新 Bot</Button>} />
}

function OverviewSection({ title, count, children }: { title: string; count: number; children: ReactNode }) {
  return <section className="grid gap-3" aria-labelledby={`tasks-${title}`}>
    <div className="flex items-center gap-2 px-1"><h2 id={`tasks-${title}`} className="text-heading font-semibold">{title}</h2><CountBadge count={count} label={`${count} 项`} className="bg-muted text-muted-foreground" /></div>
    <div className="grid gap-3">{children}</div>
  </section>
}

function TaskOverviewCard({ work, projected, lookup, connected, progress }: { work: OpenWorkCard; projected?: ProjectedWork; lookup: { nodes: NodeInfo[]; directory: MembershipView | null }; connected: boolean; progress: string }) {
  const state = cardState(work, lookup.nodes, projected?.state)
  const stale = taskStale(state, connected)
  const unknown = state === "unknown" || state === "unverified" || state === "accepted_waiting_sync" || state === "dispatch_rejected" || state === "dispatch_unverified"
  const label = stale || state === "unknown" ? staleTaskLabel : taskLabel(state)
  const tone = stale ? "neutral" : stateTone(state)
  const failed = needsLook(state) && state !== "dispatch_unverified"
  const Icon = failed ? CircleX : stale || unknown ? CircleQuestionMark : taskActive(state) ? LoaderCircle : ListChecks
  const DeviceIcon = Laptop
  const line = needsLook(state) ? workFailure(work) || (failed ? "这个任务没有成功。" : "") : progress
  const device = nodeName(work.target_node, lookup, "未知设备")
  const project = work.cwd ? projectName(work.cwd) : "未指定项目"
  const iconSurface = failed ? "bg-danger-soft text-danger" : stale || unknown ? "bg-warning-soft text-warning" : taskActive(state) ? "bg-brand-soft text-brand" : "bg-muted text-muted-foreground"
  const card = <Card className="min-w-0 border-transparent shadow-sm ring-1 ring-border/70 transition-[transform,box-shadow,background-color] duration-200 group-hover:-translate-y-0.5 group-hover:shadow-md max-md:min-h-11" data-navigable={projected?.navigation ? true : undefined}>
      <CardContent className="grid min-w-0 gap-3">
        <div className="flex min-w-0 items-start gap-3">
          <span className={cn("grid size-9 shrink-0 place-items-center rounded-lg ring-1 ring-inset ring-black/5", iconSurface)}><Icon aria-hidden="true" className={taskActive(state) && !stale ? "size-5 animate-spin motion-reduce:animate-none" : "size-5"} /></span>
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 flex-wrap items-start gap-2"><h3 className="min-w-0 flex-1 [overflow-wrap:anywhere] font-medium">{work.title || "任务"}</h3><StatusPill tone={tone}>{label}</StatusPill></div>
            <p className="mt-1 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-meta text-muted-foreground">
              <span className="inline-flex min-w-0 items-center gap-1.5"><Bot aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate">{projected?.bot?.name || "已删除的 Bot"}</span></span>
              <span aria-hidden="true">·</span>
              <span className="inline-flex min-w-0 items-center gap-1.5"><DeviceIcon aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={device}>{device}</span></span>
              <span aria-hidden="true">·</span>
              <span className="inline-flex min-w-0 items-center gap-1.5"><FolderOpen aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={work.cwd || "未指定项目"}>{project}</span></span>
            </p>
          </div>
          <ChevronRight aria-hidden="true" className="mt-1 size-4 shrink-0 text-muted-foreground transition-transform duration-200 group-hover:translate-x-0.5" />
        </div>
        {line ? <p className={cn("border-t border-border/70 pt-3 line-clamp-2 whitespace-pre-wrap break-words text-meta", failed ? "text-danger" : "text-muted-foreground")}>{line}</p> : null}
      </CardContent>
    </Card>
  return projected?.navigation?.work_id ? <Link to="/b/$botId/work/$workId" params={{ botId: projected.navigation.bot_id, workId: projected.navigation.work_id }} search={{ from: "tasks" }} className="group block min-w-0 rounded-xl outline-none focus-visible:ring-3 focus-visible:ring-ring/50">{card}</Link>
    : projected?.navigation?.bot_id ? <Link to="/b/$botId" params={{ botId: projected.navigation.bot_id }} className="group block min-w-0 rounded-xl outline-none focus-visible:ring-3 focus-visible:ring-ring/50">{card}</Link> : card
}
