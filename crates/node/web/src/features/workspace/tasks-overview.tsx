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
import { cardState, needsLook, nodeName, overviewScope, staleTaskLabel, taskActive, taskGroup, taskGroups, taskLabel, taskProgress, taskStale, unrepresentedOverviewChecks, type TaskGroup } from "@vibeface/client/workspace/labels.ts"
import { projectName, workApproval } from "@vibeface/client/workspace/questions.ts"
import { workFailure } from "@vibeface/client/lib/errors.ts"
import type { NodeInfo, OpenWork, OverviewTask } from "@vibeface/client/workspace/types.ts"
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
  const overviewWorks = openWorksFromOverview(overview, workspace.openWorks)
  const read = workspace.synced
  const projected = new Map([...localOverview.tasks, ...overview.tasks].filter((task): task is ProjectedWork => (task.kind === "work" || task.kind === "dispatch") && Boolean(task.work_id)).map((task) => [JSON.stringify([task.conversation_id, task.work_id]), task]))
  const projection = (work: OpenWork) => projected.get(JSON.stringify([work.conversation_id, work.work_id]))
  const needsConfirmation = (check: { conversation_id: string; work_id: string; target_node: string }) => {
    const work = overviewWorks.find((work) => work.conversation_id === check.conversation_id && work.work_id === check.work_id && work.target_node === check.target_node)
    return !work || !["completed", "failed", "interrupted", "rejected"].includes(cardState(work, workspace.nodes, projection(work)))
  }
  const attention = inbox.filter((item): item is Extract<InboxItem, { kind: "question" | "confirm" }> => item.kind === "confirm" || item.kind === "question" && !item.done).flatMap((item): Extract<InboxItem, { kind: "question" | "confirm" }>[] => {
    if (item.kind !== "confirm" || item.check.kind === "process") return [item]
    if (item.check.kind === "work") return needsConfirmation(item.check) ? [item] : []
    const works = item.check.works.filter(needsConfirmation)
    return works.length ? [{ ...item, check: { ...item.check, works } }] : []
  })
  const attentionWorkIds = new Set<string>()
  for (const item of attention) {
    if (item.kind !== "confirm") continue
    if (item.check.kind === "work") attentionWorkIds.add(JSON.stringify([item.check.conversation_id, item.check.work_id]))
    if (item.check.kind === "device") for (const work of item.check.works) attentionWorkIds.add(JSON.stringify([work.conversation_id, work.work_id]))
  }
  const openQuestions = attention.filter((item): item is Extract<InboxItem, { kind: "question" }> => item.kind === "question").map((item) => item.question)
  const tasks = overviewWorks.filter((work) => !attentionWorkIds.has(JSON.stringify([work.conversation_id, work.work_id])))
  const groups: Record<TaskGroup, OpenWork[]> = { attention: [], working: [], waiting: [] }
  const botIds = new Map(workspace.bots.map((bot) => [bot.conversation_id, bot.bot_id]))
  const matchesBot = (conversationId: string, projectedBotId?: string | null) => botFilter === "all" || projectedBotId === botFilter || botIds.get(conversationId) === botFilter
  for (const work of tasks) {
    const item = projection(work)
    const state = cardState(work, workspace.nodes, item)
    const asking = state === "running" && workApproval({ work, conversationId: work.conversation_id, questions: openQuestions, answered: workspace.answered })
    if (matchesBot(work.conversation_id, item?.bot?.bot_id)) groups[taskGroup(taskStale(state, workspace.connectionOnline && read) ? "unknown" : state, Boolean(asking))].push(work)
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
  const otherChecks = unrepresentedOverviewChecks(overview, overviewWorks, workspace.nodes, read ? workspace.bots : []).filter((task) => task.conversation_id !== workspace.conversationId && matchesBot(task.conversation_id, task.bot?.bot_id))
  const visible = (group: TaskGroup) => filter === "all" || filter === group
  const noticed = visible("attention") ? visibleAttention.length + groups.attention.length + otherChecks.length : 0
  const visibleCount = noticed + (visible("working") ? groups.working.length : 0) + (visible("waiting") ? groups.waiting.length : 0)
  const hasContent = visibleCount > 0
  const emptyLabel = !read ? workspace.connectionOnline ? "正在读取当前连接的任务" : "尚未读取任务，连接后再确认" : filter === "all" && botFilter === "all" ? "当前记录没有未完成的任务" : "没有符合筛选的任务"
  const card = (work: OpenWork) => <TaskOverviewCard key={JSON.stringify([work.conversation_id, work.work_id])} work={work} projected={projection(work)} bot={workspace.bots.find((bot) => bot.conversation_id === work.conversation_id)} waiting={Boolean(workApproval({ work, conversationId: work.conversation_id, questions: openQuestions, answered: workspace.answered }))} lookup={lookup} connected={workspace.connectionOnline && read} progress={taskProgress(work, workspace.liveProgress)} />
  const bot = workspace.bots.find((item) => item.bot_id === botFilter) ?? workspace.selectedBot ?? workspace.bots[0]

  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <p className="sr-only" role="status" aria-live="polite">{hasContent ? `${visibleCount} 项任务` : emptyLabel}</p>
    <WorkspaceMasthead attention={read ? visibleAttention.length + groups.attention.length + otherChecks.length : null} working={read ? groups.working.length : null} waiting={read ? groups.waiting.length : null} online={workspace.connectionOnline} />
    <p className="text-meta text-muted-foreground">{[...overviewScope(overview.coverage), ...(!read ? ["当前连接尚未完成首读"] : !workspace.connectionOnline ? ["连接已断开，显示最近记录"] : [])].join(" · ")}</p>
    <TaskFilters filter={filter} botFilter={botFilter} bots={workspace.bots} onFilter={setFilter} onBotFilter={setBotFilter} />
    {taskGroups.map(({ group, title }) => {
      if (!visible(group)) return null
      const count = group === "attention" ? noticed : groups[group].length
      return count ? <OverviewSection key={group} title={title} count={count}>
        {group === "attention" ? visibleAttention.map((item) => <div key={item.key}>{item.kind === "question" ? <QuestionCard question={item.question} inbox from="tasks" /> : <ConfirmCard check={item.check} inbox from="tasks" />}</div>) : null}
        {group === "attention" ? otherChecks.map((check) => <Card key={`${check.conversation_id}:${check.item_id}`}><CardContent className="grid gap-2"><p className="font-medium">{check.work_id ? "任务状态待确认" : "程序状态待确认"}</p><p className="text-meta text-muted-foreground">{check.bot?.name ?? (read ? "未找到所属 Bot" : "Bot 身份未加载")}{check.bot ? " · 请到所属 Bot 查看" : ""}</p>{check.bot ? <Button asChild variant="outline"><Link to="/b/$botId" params={{ botId: check.bot.bot_id }}>打开 {check.bot.name}</Link></Button> : null}</CardContent></Card>) : null}
        {groups[group].map(card)}
      </OverviewSection> : null
    })}
    {!hasContent && !read && workspace.connectionOnline ? <div aria-busy="true" className="grid gap-3"><Skeleton className="h-20" /><Skeleton className="h-20" /></div> : !hasContent ? <WorkspaceEmpty title={emptyLabel} bot={bot} onCreate={createBot} /> : null}
  </div>
}

function WorkspaceMasthead({ attention, working, waiting, online }: { attention: number | null; working: number | null; waiting: number | null; online: boolean }) {
  const lead = attention === null ? "当前连接的任务数量尚未确认。" : attention ? `${attention} 项需要你先处理，其他工作会继续留在这里。` : working ? `${working} 项正在推进，等待中的工作会单独留在视线里。` : "从每个 Bot 的下一步开始。"
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

function Metric({ label, value, tone }: { label: string; value: number | null; tone: Tone }) {
  return <div className="grid min-w-0 gap-1 rounded-xl border border-border/70 bg-background/55 px-3 py-2.5"><span className="truncate text-meta text-muted-foreground">{label}</span><span className={cn("text-xl font-semibold tabular-nums", tone === "warning" ? "text-warning" : tone === "info" ? "text-info" : "text-foreground")}>{value ?? "待确认"}</span></div>
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

function TaskOverviewCard({ work, projected, bot, waiting, lookup, connected, progress }: { work: OpenWork; projected?: ProjectedWork; bot?: { bot_id: string; name: string }; waiting: boolean; lookup: { nodes: NodeInfo[]; directory: MembershipView | null }; connected: boolean; progress: string }) {
  const shown = cardState(work, lookup.nodes, projected)
  const state = shown === "running" && waiting ? "waiting" : shown
  const stale = taskStale(state, connected)
  const unknown = state === "unknown" || state === "unverified" || state === "accepted_waiting_sync" || state === "dispatch_rejected" || state === "dispatch_unverified"
  const label = stale || state === "unknown" ? staleTaskLabel : taskLabel(state)
  const tone = stale ? "neutral" : stateTone(state)
  const failed = needsLook(state) && state !== "dispatch_unverified"
  const Icon = failed ? CircleX : stale || unknown ? CircleQuestionMark : taskActive(state) ? LoaderCircle : ListChecks
  const DeviceIcon = Laptop
  const failure = work.outcome === state || work.state === state ? work : { failure: projected?.failure, failure_reason: projected?.reason ?? null }
  const line = needsLook(state) ? workFailure(failure) || (failed ? "这个任务没有成功。" : "") : progress
  const device = nodeName(work.target_node, lookup, "未知设备")
  const project = work.cwd ? projectName(work.cwd) : "未指定项目"
  const iconSurface = failed ? "bg-danger-soft text-danger" : stale || unknown ? "bg-warning-soft text-warning" : taskActive(state) ? "bg-brand-soft text-brand" : "bg-muted text-muted-foreground"
  const navigation = projected?.navigation ?? (bot ? { bot_id: bot.bot_id, work_id: work.work_id } : null)
  const card = <Card className="min-w-0 border-transparent shadow-sm ring-1 ring-border/70 transition-[transform,box-shadow,background-color] duration-200 group-hover:-translate-y-0.5 group-hover:shadow-md max-md:min-h-11" data-navigable={navigation ? true : undefined}>
      <CardContent className="grid min-w-0 gap-3">
        <div className="flex min-w-0 items-start gap-3">
          <span className={cn("grid size-9 shrink-0 place-items-center rounded-lg ring-1 ring-inset ring-black/5", iconSurface)}><Icon aria-hidden="true" className={taskActive(state) && !stale ? "size-5 animate-spin motion-reduce:animate-none" : "size-5"} /></span>
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 flex-wrap items-start gap-2"><h3 className="min-w-0 flex-1 [overflow-wrap:anywhere] font-medium">{work.title || "任务"}</h3><StatusPill tone={tone}>{label}</StatusPill></div>
            <p className="mt-1 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-meta text-muted-foreground">
              <span className="inline-flex min-w-0 items-center gap-1.5"><Bot aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate">{bot?.name || projected?.bot?.name || "已删除的 Bot"}</span></span>
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
  return navigation?.work_id ? <Link to="/b/$botId/work/$workId" params={{ botId: navigation.bot_id, workId: navigation.work_id }} search={{ from: "tasks" }} className="group block min-w-0 rounded-xl outline-none focus-visible:ring-3 focus-visible:ring-ring/50">{card}</Link>
    : navigation?.bot_id ? <Link to="/b/$botId" params={{ botId: navigation.bot_id }} className="group block min-w-0 rounded-xl outline-none focus-visible:ring-3 focus-visible:ring-ring/50">{card}</Link> : card
}
