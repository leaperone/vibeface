import { RoutedAction } from "@/components/arc/routed-action"
import taskStyles from "@/components/arc/task-compositions.module.css"
import { TaskRunSummary } from "@/components/arc/task-run-summary"
import { Select } from "@vibeface/uiarc/select/select.tsx"
"use client"

import { CountBadge,Details,EmptyState,InlineError,Segmented,Skeleton,StatusPill,type Tone } from "@/components/arc/product-kit"
import { RecordBody,RecordPanel } from "@/components/arc/record-panel"
import { cn } from "@/lib/utils"
import { Link } from "@tanstack/react-router"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { workFailure } from "@vibeface/client/lib/errors.ts"
import type { MembershipView } from "@vibeface/client/lib/node.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { cardState,needsLook,nodeName,overviewScope,schedulerStatus,staleTaskLabel,taskActive,taskGroups,taskLabel,taskProgress,taskStale,workspaceActivities,type TaskGroup } from "@vibeface/client/workspace/labels.ts"
import { openWorksFromOverview,overviewFromWorkspace } from "@vibeface/client/workspace/model.ts"
import { projectName } from "@vibeface/client/workspace/questions.ts"
import type { NodeInfo,OpenWorkCard,OverviewTask,WorkspaceActivity } from "@vibeface/client/workspace/types.ts"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Bot,ChevronRight,CircleQuestionMark,CircleX,FolderOpen,Laptop,ListChecks,LoaderCircle,MessageCircle,Plus } from "lucide-react"
import type { ReactNode } from "react"
import { useCallback,useState } from "react"
import { useInbox } from "./inbox"
import { PendingSection } from "./pending-section"
import { useCreateBot } from "./workspace-shell"
import { WorkspaceTasks } from "./workspace-tasks"

type ProjectedWork = Extract<OverviewTask, { kind: "work" | "dispatch" }>

const stateTone = (state: string): Tone => state === "failed" || state === "dispatch_rejected" ? "danger" : state === "completed" ? "success" : ["waiting", "awaiting_confirmation", "unverified", "unknown", "accepted_waiting_sync", "dispatch_unverified"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"

/** The personal task surface: 待处理 on top, then tasks. Questions and checks act only in 待处理; a task held on one shows it as waiting. */
export function TasksOverview() {
  const workspace = useWorkspace()
  const createBot = useCreateBot()
  const { directory } = useDevice()
  const [filter, setFilter] = useState<TaskGroup | "all">("all")
  const [botFilter, setBotFilter] = useState("all")
  const [readFailure, setReadFailure] = useState<{ generation: number; cause: unknown; kind: "workspace" | "tasks" } | null>(null)
  const handleReadError = useCallback((cause: unknown) => setReadFailure({ generation: workspace.generation, cause, kind: "tasks" }), [workspace.generation])
  const inbox = useInbox()
  const failedIds = new Set(inbox.flatMap((item) => item.kind === "failed" ? [item.work.work_id] : []))
  type InboxItem = (typeof inbox)[number]
  const lookup = { nodes: workspace.nodes, directory }
  const localOverview = overviewFromWorkspace({ openWorks: workspace.openWorks, bots: workspace.bots, questions: workspace.questions, nodes: workspace.nodes })
  const overview = workspace.overview ?? localOverview
  const overviewWorks = openWorksFromOverview(overview, workspace.openWorks)
  // The task snapshot does not wait for conversation reads or write alignment.
  const read = Boolean(workspace.synced || workspace.nodesRead)
  const projected = new Map([...localOverview.tasks, ...overview.tasks].filter((task): task is ProjectedWork => (task.kind === "work" || task.kind === "dispatch") && Boolean(task.work_id)).map((task) => [JSON.stringify([task.conversation_id, task.work_id]), task]))
  const projection = (work: OpenWorkCard) => projected.get(JSON.stringify([work.conversation_id, work.work_id]))
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
  const openQuestions = attention.filter((item): item is Extract<InboxItem, { kind: "question" }> => item.kind === "question").map((item) => item.question)
  const pendingChecks = attention.filter((item): item is Extract<InboxItem, { kind: "confirm" }> => item.kind === "confirm").map((item) => item.check)
  const activityView = workspaceActivities({
    works: overviewWorks, questions: openQuestions, checks: pendingChecks, overview,
    jobs: (workspace.data?.scheduler?.jobs ?? []).map((job) => ({ conversation_id: workspace.conversationId, job })),
    answered: workspace.answered, nodes: workspace.nodes, knownBots: read ? workspace.bots : [], connected: workspace.connectionOnline, read,
  })
  const groups: Record<TaskGroup, WorkspaceActivity[]> = activityView.groups
  const botIds = new Map(workspace.bots.map((bot) => [bot.conversation_id, bot.bot_id]))
  const matchesBot = (conversationId: string, projectedBotId?: string | null) => botFilter === "all" || projectedBotId === botFilter || botIds.get(conversationId) === botFilter
  const activityBot = (activity: WorkspaceActivity) => activity.conversation_id ? workspace.bots.find((bot) => bot.conversation_id === activity.conversation_id) ?? overview.bots.find((bot) => bot.conversation_id === activity.conversation_id) : undefined
  const taskStates: Record<TaskGroup, string[]> = { attention: ['waiting_user', 'needs_reconciliation', 'update_pending', 'stop_pending'], working: ['running'], waiting: ['not_started', 'waiting_dependencies', 'deferred'] }
  const businessTasks = (workspace.workspaceState?.tasks ?? []).filter((task) => botFilter === 'all' || task.owner_bot === botFilter)
  const taskCounts = { attention: businessTasks.filter((task) => taskStates.attention.includes(task.state)).length, working: businessTasks.filter((task) => taskStates.working.includes(task.state)).length, waiting: businessTasks.filter((task) => taskStates.waiting.includes(task.state)).length }
  const assignedWorkIds = new Set((workspace.workspaceState?.assignments ?? []).flatMap((assignment) => assignment.work_id && workspace.workspaceState?.tasks.some((task) => task.task_id === assignment.task_id) ? [assignment.work_id] : []))
  const visibleGroups: Record<TaskGroup, WorkspaceActivity[]> = { attention: [], working: [], waiting: [] }
  const heldGroups: Record<TaskGroup, number> = { attention: 0, working: 0, waiting: 0 }
  for (const group of taskGroups.map(({ group }) => group)) for (const activity of groups[group]) {
    if (activity.kind === 'work' && activity.work && assignedWorkIds.has(activity.work.work_id)) continue
    const deviceMatch = activity.kind === "attention" && "kind" in activity.attention && activity.attention.kind === "device" && activity.attention.works.some((work) => matchesBot(work.conversation_id))
    if (!matchesBot(activity.conversation_id ?? "", activityBot(activity)?.bot_id) && !deviceMatch) continue
    // A failed work, live question or check is acted on in 待处理 above: counted here, shown only there.
    if (activity.kind === "work" && activity.work && failedIds.has(activity.work.work_id) || activity.kind === "attention" && !("kind" in activity.attention && (activity.attention.kind === "question" || activity.attention.kind === "check"))) heldGroups[group]++
    else visibleGroups[group].push(activity)
  }
  const visible = (group: TaskGroup) => filter === "all" || filter === group
  const visibleCount = (visible("attention") ? visibleGroups.attention.length + heldGroups.attention : 0) + (visible("working") ? visibleGroups.working.length + heldGroups.working : 0) + (visible("waiting") ? visibleGroups.waiting.length + heldGroups.waiting : 0) + businessTasks.filter((task) => filter === "all" || taskStates[filter].includes(task.state)).length
  const hasContent = visibleCount > 0
  const tasksRead = read && Boolean(workspace.workspaceState)
  // Test all loaded records before filters, including history and work held in 待处理.
  const hasTaskRecords = Boolean(workspace.workspaceState?.tasks.length || overviewWorks.length || overview.tasks.length || activityView.activities.length
    || inbox.some((item) => ["question", "confirm", "failed", "undelivered", "attention", "session"].includes(item.kind)))
  const globallyEmpty = tasksRead && !hasTaskRecords
  const localReadError = readFailure && readFailure.generation === workspace.generation && (readFailure.kind === "workspace" ? !read : !workspace.workspaceState) ? readFailure.cause : null
  // A successful refresh leaves the global toast object in place; it is no longer a first-read failure once synced.
  const readError = localReadError ?? (!read ? workspace.error?.text ?? null : null)
  // Stage the entire first read, including saved records. Errors/offline release known work immediately.
  const loading = !tasksRead && (!hasTaskRecords || workspace.connectionOnline && !readError)
  const pendingVisible = !loading || !workspace.connectionOnline || Boolean(readError)
  const scope = [...overviewScope(overview.coverage), ...(!read ? ["当前连接尚未完成首读"] : !workspace.connectionOnline ? ["连接已断开，显示最近记录"] : [])].join(" · ")
  const emptyLabel = !tasksRead ? workspace.connectionOnline ? "正在读取当前连接的任务" : "尚未读取任务，连接后再确认" : filter === "all" && botFilter === "all" ? "当前记录没有未完成的任务" : "没有符合筛选的任务"
  const card = (activity: WorkspaceActivity) => {
    if (activity.kind === "work" && activity.work) return <div key={activity.id} className="grid gap-2"><TaskOverviewCard work={activity.work} projected={activity.projection} bot={activityBot(activity)} waiting={activity.facts.questions.length > 0} lookup={lookup} connected={workspace.connectionOnline && read} progress={taskProgress(activity.work, workspace.liveProgress)} />{activity.facts.overviewChecks.map((check) => <OverviewCheckCard key={`overview-check:${check.check_id}`} check={check} />)}</div>
    if (activity.kind === "turn" && activity.job) return <TurnActivity key={activity.id} activity={activity} lookup={lookup} />
    if (activity.kind === "attention" && activity.attention) return <AttentionActivity key={activity.id} activity={activity} bot={activityBot(activity)} read={read} />
    return null
  }
  const bot = workspace.bots.find((item) => item.bot_id === botFilter) ?? workspace.selectedBot ?? workspace.bots[0]

  function retryRead() {
    if (!workspace.isCurrent()) return
    setReadFailure(null)
    const generation = workspace.generation, kind = read ? "tasks" : "workspace"
    const attempt = read ? workspace.readWorkspaceState() : workspace.refresh()
    void attempt.catch((cause) => { if (workspace.isCurrent()) setReadFailure({ generation, kind, cause }) })
  }

  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <div className={pendingVisible ? "contents" : "hidden"}><PendingSection /></div>
    {loading || !read && readError ? <div aria-busy={loading && workspace.connectionOnline && !readError || undefined} className="grid gap-3">
      <p role="status" className="text-sm text-secondary">{readError ? hasTaskRecords ? "任务记录还没读全" : "任务还没读到" : emptyLabel}</p>
      <p className="text-sm text-secondary">{scope}</p>
      <InlineError error={readError} action={{ label: "重新读取", onClick: retryRead }} />
      {loading && workspace.connectionOnline && !readError ? <><Skeleton className="h-20" /><Skeleton className="h-20" /></> : null}
    </div> : null}
    <div className={loading ? "hidden" : "contents"}>
      <p className="sr-only" role="status" aria-live="polite">{hasContent ? `${visibleCount} 项任务` : emptyLabel}</p>
      {globallyEmpty ? null : <TaskRunSummary attention={read ? visibleGroups.attention.length + heldGroups.attention + taskCounts.attention : null} working={read ? visibleGroups.working.length + heldGroups.working + taskCounts.working : null} waiting={read ? visibleGroups.waiting.length + heldGroups.waiting + taskCounts.waiting : null} online={workspace.connectionOnline} />}
      <p className="text-sm text-secondary">{scope}</p>
      {globallyEmpty ? null : <TaskFilters filter={filter} botFilter={botFilter} bots={workspace.bots} onFilter={setFilter} onBotFilter={setBotFilter} />}
      <WorkspaceTasks botFilter={botFilter} filter={filter} onReadError={handleReadError} showEmpty={globallyEmpty} />
      {taskGroups.map(({ group, title }) => {
        if (!visible(group)) return null
        const count = visibleGroups[group].length
        return count ? <OverviewSection key={group} title={title} count={count}>
          {visibleGroups[group].map(card)}
        </OverviewSection> : null
      })}
      {globallyEmpty && !workspace.bots.length ? <div><Button type="button" onClick={createBot}><Plus aria-hidden="true" />新 Bot</Button></div> : null}
      {!globallyEmpty && !loading && !hasContent ? <WorkspaceEmpty title={emptyLabel} bot={bot} onCreate={createBot} onClear={filter !== "all" || botFilter !== "all" ? () => { setFilter("all"); setBotFilter("all") } : undefined} /> : null}
    </div>
  </div>
}

function OverviewCheckCard({ check, read = true }: { check: Extract<OverviewTask, { kind: "check" }>; read?: boolean }) {
  return <RecordPanel><RecordBody className="grid gap-2"><p className="font-medium">{check.work_id ? "任务状态待确认" : "程序状态待确认"}</p><p className="text-sm text-secondary">{check.bot?.name ?? (read ? "未找到所属 Bot" : "Bot 身份未加载")}{check.bot ? " · 请到所属 Bot 查看" : ""}</p>{check.bot ? <RoutedAction variant="secondary" to="/b/$botId" params={{ botId: check.bot.bot_id }}>打开 {check.bot.name}</RoutedAction> : null}</RecordBody></RecordPanel>
}

function TurnActivity({ activity, lookup }: { activity: Extract<WorkspaceActivity, { kind: "turn" }>; lookup: { nodes: NodeInfo[]; directory: MembershipView | null } }) {
  const job = activity.job
  if (!job) return null
  const purpose = job.trigger.startsWith("message:") ? "处理消息" : job.trigger.startsWith("task:") ? "推进任务" : job.trigger.startsWith("schedule:") ? "定时工作" : "后台工作"
  return <RecordPanel className="min-w-0"><RecordBody className="grid min-w-0 gap-2 [overflow-wrap:anywhere]">
    <p className="font-medium">{purpose}</p>
    <p className="text-sm text-secondary">{schedulerStatus(job.state).label}</p>
    {job.source_node ? <p className="text-sm text-secondary">来源设备 · {nodeName(job.source_node, lookup, "未知设备")}</p> : null}
    <Details label="技术详情" className="min-w-0 [overflow-wrap:anywhere]"><dl className="grid min-w-0 gap-1 text-sm"><dt>状态标识</dt><dd>{job.state}</dd><dt>触发记录</dt><dd>{job.trigger || "未提供"}</dd>{job.source_node ? <><dt>来源设备标识</dt><dd>{job.source_node}</dd></> : null}</dl></Details>
  </RecordBody></RecordPanel>
}

function AttentionActivity({ activity, bot, read }: { activity: Extract<WorkspaceActivity, { kind: "attention" }>; bot?: { bot_id: string; name: string }; read: boolean }) {
  const attention = activity.attention
  if (!attention) return null
  if ("kind" in attention && attention.kind === "check") return <OverviewCheckCard check={attention} read={read} />
  if ("question_id" in attention) return <RecordPanel><RecordBody className="grid gap-2"><p className="font-medium">问题待确认</p><p className="text-sm text-secondary">{attention.text}</p>{bot ? <RoutedAction variant="secondary" to="/b/$botId" params={{ botId: bot.bot_id }}>打开 {bot.name}</RoutedAction> : <p className="text-sm text-secondary">{read ? "未找到所属 Bot" : "Bot 身份未加载"}</p>}</RecordBody></RecordPanel>
  return null
}

function TaskFilters({ filter, botFilter, bots, onFilter, onBotFilter }: { filter: TaskGroup | "all"; botFilter: string; bots: readonly { bot_id: string; name: string }[]; onFilter: (value: TaskGroup | "all") => void; onBotFilter: (value: string) => void }) {
  const options = [{ value: "all" as const, label: "全部" }, ...taskGroups.map(({ group, title }) => ({ value: group, label: title }))]
  return <section className={taskStyles.filters} aria-label="任务筛选">
    <Segmented label="任务状态" value={filter} options={options} onChange={onFilter} />
    <Select label="按 Bot 筛选"  value={"arc:" + botFilter} onValueChange={selected => { const value = selected.slice(4);  onBotFilter(value)  }} options={[{ value: "arc:all", label: "所有 Bot" }, ...bots.map((bot) => ({ value: "arc:" + bot.bot_id, label: bot.name }))]} />
  </section>
}

function WorkspaceEmpty({ title, bot, onCreate, onClear }: { title: string; bot?: { bot_id: string; name: string }; onCreate: () => void; onClear?: () => void }) {
  return <EmptyState icon={ListChecks} title={title} action={onClear ? <Button type="button" variant="secondary" onClick={onClear}>清除筛选</Button> : bot ? <RoutedAction variant="secondary" to="/b/$botId" params={{ botId: bot.bot_id }}><MessageCircle aria-hidden="true" />打开 {bot.name}</RoutedAction> : <Button type="button" onClick={onCreate}><Plus aria-hidden="true" />新 Bot</Button>} />
}

function OverviewSection({ title, count, children }: { title: string; count: number; children: ReactNode }) {
  return <section className="grid gap-3" aria-labelledby={`tasks-${title}`}>
    <div className="flex items-center gap-2 px-1"><h2 id={`tasks-${title}`} className="text-lg font-medium">{title}</h2><CountBadge count={count} label={`${count} 项`} className="bg-surface-muted text-secondary" /></div>
    <div className="grid gap-3">{children}</div>
  </section>
}

function TaskOverviewCard({ work, projected, bot, waiting, lookup, connected, progress }: { work: OpenWorkCard; projected?: ProjectedWork; bot?: { bot_id: string; name: string }; waiting: boolean; lookup: { nodes: NodeInfo[]; directory: MembershipView | null }; connected: boolean; progress: string }) {
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
  const iconSurface = failed ? "text-danger" : stale || unknown ? "text-warning" : taskActive(state) ? "text-accent-strong" : "text-secondary"
  const navigation = projected?.navigation ?? (bot ? { bot_id: bot.bot_id, work_id: work.work_id } : null)
  const card = <RecordPanel className="min-w-0" data-navigable={navigation ? true : undefined}>
      <RecordBody className="grid min-w-0 gap-3">
        <div className="flex min-w-0 items-start gap-3">
          <span className={cn("flex size-6 shrink-0 items-center justify-center", iconSurface)}><Icon aria-hidden="true" className={taskActive(state) && !stale ? "size-5 animate-spin motion-reduce:animate-none" : "size-5"} /></span>
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 flex-wrap items-start gap-2"><h3 className="min-w-0 flex-1 [overflow-wrap:anywhere] font-medium">{work.title || "任务"}</h3><StatusPill tone={tone}>{label}</StatusPill></div>
            <p className="mt-1 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-sm text-secondary">
              <span className="inline-flex min-w-0 items-center gap-1.5"><Bot aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate">{bot?.name || projected?.bot?.name || "已删除的 Bot"}</span></span>
              <span aria-hidden="true">·</span>
              <span className="inline-flex min-w-0 items-center gap-1.5"><DeviceIcon aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={device}>{device}</span></span>
              <span aria-hidden="true">·</span>
              <span className="inline-flex min-w-0 items-center gap-1.5"><FolderOpen aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={work.cwd || "未指定项目"}>{project}</span></span>
            </p>
          </div>
          <ChevronRight aria-hidden="true" className="mt-1 size-4 shrink-0 text-secondary transition-transform duration-200 group-hover:translate-x-0.5" />
        </div>
        {line ? <p className={cn("border-t border-border/70 pt-3 line-clamp-2 whitespace-pre-wrap break-words text-sm", failed ? "text-danger" : "text-secondary")}>{line}</p> : null}
      </RecordBody>
    </RecordPanel>
  return navigation?.work_id ? <Link to="/b/$botId/work/$workId" params={{ botId: navigation.bot_id, workId: navigation.work_id }} search={{ from: "tasks" }} className="group block min-w-0 rounded-panel outline-none  ">{card}</Link>
    : navigation?.bot_id ? <Link to="/b/$botId" params={{ botId: navigation.bot_id }} className="group block min-w-0 rounded-panel outline-none  ">{card}</Link> : card
}
