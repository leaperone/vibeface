"use client"

import type { ReactNode } from "react"
import { Link } from "@tanstack/react-router"
import { Bot, ChevronRight, CircleQuestionMark, CircleX, FolderOpen, Laptop, ListChecks, LoaderCircle, MessageCircle, Plus, Server } from "lucide-react"
import { Card, CardContent } from "@/components/ui/card"
import { Button } from "@/components/ui/button"
import { CountBadge, EmptyState, Skeleton, StatusPill, type Tone } from "@/components/app/kit"
import { cn } from "@/lib/utils"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import type { MembershipView } from "@vibeface/client/lib/node.ts"
import { openWorksFromOverview, overviewFromWorkspace } from "@vibeface/client/workspace/model.ts"
import { cardState, needsLook, nodeName, staleTaskLabel, taskActive, taskGroup, taskGroups, taskLabel, taskProgress, taskStale, type TaskGroup } from "@vibeface/client/workspace/labels.ts"
import { projectName } from "@vibeface/client/workspace/questions.ts"
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
  const groups: Record<TaskGroup, OpenWork[]> = { attention: [], working: [], waiting: [] }
  for (const work of tasks) groups[taskGroup(cardState(work, workspace.nodes, projected.get(work.work_id)?.state), waitingSessions.has(work.thread_id ?? ""))].push(work)
  const noticed = attention.length + groups.attention.length
  const hasContent = noticed > 0 || groups.working.length > 0 || groups.waiting.length > 0
  const card = (work: OpenWork) => <TaskOverviewCard key={work.work_id} work={work} projected={projected.get(work.work_id)} lookup={lookup} readyDevice={ready?.entry.device_id} connected={workspace.connectionOnline} progress={taskProgress(work, workspace.liveProgress)} />
  const bot = workspace.selectedBot ?? workspace.bots[0]

  if (workspace.incomplete && !workspace.nodes.length) return <div className="mx-auto grid w-full max-w-(--page-width) gap-3" aria-busy="true"><Skeleton className="h-20" /><Skeleton className="h-20" /><Skeleton className="h-20" /></div>
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <p className="sr-only" role="status" aria-live="polite">{hasContent ? `${attention.length + tasks.length} 项任务` : "没有未完成的任务"}</p>
    {taskGroups.map(({ group, title }) => {
      const count = group === "attention" ? noticed : groups[group].length
      return count ? <OverviewSection key={group} title={title} count={count}>
        {group === "attention" ? attention.map((item) => <div key={item.key}>{item.kind === "question" ? <QuestionCard question={item.question} inbox from="tasks" /> : <ConfirmCard check={item.check} inbox from="tasks" />}</div>) : null}
        {groups[group].map(card)}
      </OverviewSection> : null
    })}
    {!hasContent ? <WorkspaceEmpty bot={bot} onCreate={createBot} /> : null}
  </div>
}

function WorkspaceEmpty({ bot, onCreate }: { bot?: { bot_id: string; name: string }; onCreate: () => void }) {
  return <EmptyState icon={ListChecks} title="没有未完成的任务" action={bot ? <Button asChild variant="outline"><Link to="/b/$botId" params={{ botId: bot.bot_id }}><MessageCircle aria-hidden="true" />打开 {bot.name}</Link></Button> : <Button type="button" onClick={onCreate}><Plus aria-hidden="true" />新 Bot</Button>} />
}

function OverviewSection({ title, count, children }: { title: string; count: number; children: ReactNode }) {
  return <section className="grid gap-3" aria-labelledby={`tasks-${title}`}>
    <div className="flex items-center gap-2 px-1"><span aria-hidden="true" className="h-5 w-1 rounded-full bg-brand" /><h2 id={`tasks-${title}`} className="text-heading font-semibold">{title}</h2><CountBadge count={count} label={`${count} 项`} /></div>
    <div className="grid gap-3">{children}</div>
  </section>
}

function TaskOverviewCard({ work, projected, lookup, readyDevice, connected, progress }: { work: OpenWork; projected?: ProjectedWork; lookup: { nodes: NodeInfo[]; directory: MembershipView | null }; readyDevice?: string; connected: boolean; progress: string }) {
  const state = cardState(work, lookup.nodes, projected?.state)
  const stale = taskStale(state, connected)
  const unknown = state === "unknown" || state === "unverified" || state === "accepted_waiting_sync" || state === "dispatch_rejected" || state === "dispatch_unverified"
  const label = stale || state === "unknown" ? staleTaskLabel : taskLabel(state)
  const tone = stale ? "neutral" : stateTone(state)
  const failed = needsLook(state) && state !== "dispatch_unverified"
  const Icon = failed ? CircleX : stale || unknown ? CircleQuestionMark : taskActive(state) ? LoaderCircle : ListChecks
  const DeviceIcon = work.target_node === readyDevice ? Laptop : Server
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
