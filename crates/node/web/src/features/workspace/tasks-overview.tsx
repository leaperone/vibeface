"use client"

import type { ReactNode } from "react"
import { Link } from "@tanstack/react-router"
import { Bot, ChevronRight, CircleQuestionMark, FolderOpen, Laptop, ListChecks, LoaderCircle, Server } from "lucide-react"
import { Card, CardContent } from "@/components/ui/card"
import { CountBadge, EmptyState, Skeleton, StatusPill, type Tone } from "@/components/app/kit"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import type { MembershipView } from "@vibeface/client/lib/node.ts"
import { nodeName, shownState, staleTaskLabel, taskActive, taskLabel, taskStale } from "@vibeface/client/workspace/labels.ts"
import { projectName } from "@vibeface/client/workspace/questions.ts"
import type { NodeInfo, OpenWork, BotRecord } from "@vibeface/client/workspace/types.ts"
import { useInbox } from "./inbox"
import { ConfirmCard } from "./task-activity"
import { QuestionCard } from "./question-card"

type TaskGroup = "working" | "waiting"

const stateTone = (state: string): Tone => state === "failed" ? "danger" : state === "completed" ? "success" : ["waiting", "awaiting_confirmation", "unverified"].includes(state) ? "warning" : taskActive(state) ? "info" : "neutral"

/** The personal task surface. It only derives display rows from the already parsed workspace context. */
export function TasksOverview() {
  const workspace = useWorkspace()
  const { directory, ready } = useDevice()
  const inbox = useInbox()
  type InboxItem = (typeof inbox)[number]
  const lookup = { nodes: workspace.nodes, directory }
  const attention = inbox.filter((item): item is Extract<InboxItem, { kind: "question" | "confirm" }> => item.kind === "confirm" || item.kind === "question" && !item.done)
  const attentionWorkIds = new Set<string>()
  for (const item of attention) {
    if (item.kind !== "confirm") continue
    if (item.check.kind === "work") attentionWorkIds.add(item.check.work_id)
    if (item.check.kind === "device") for (const work of item.check.works) attentionWorkIds.add(work.work_id)
  }
  const openQuestions = attention.filter((item): item is Extract<InboxItem, { kind: "question" }> => item.kind === "question").map((item) => item.question)
  const waitingSessions = new Set(openQuestions.filter((question) => question.about.request_id).map((question) => question.about.session_id))
  const tasks = workspace.openWorks.filter((work) => !attentionWorkIds.has(work.work_id))
  const groups: Record<TaskGroup, OpenWork[]> = { working: [], waiting: [] }
  for (const work of tasks) groups[taskGroup(work, workspace.nodes, waitingSessions)].push(work)
  const hasContent = attention.length > 0 || groups.working.length > 0 || groups.waiting.length > 0

  if (workspace.incomplete && !workspace.nodes.length) return <div className="mx-auto grid w-full max-w-(--page-width) gap-3" aria-busy="true"><Skeleton className="h-20" /><Skeleton className="h-20" /><Skeleton className="h-20" /></div>
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-7">
    <p className="sr-only" role="status" aria-live="polite">{hasContent ? `${attention.length + tasks.length} 项未完成任务` : "没有未完成任务"}</p>
    {attention.length ? <OverviewSection title="需要留意" count={attention.length}>
      {attention.map((item) => <div key={item.key}>{item.kind === "question" ? <QuestionCard question={item.question} inbox /> : <ConfirmCard check={item.check} inbox />}</div>)}
    </OverviewSection> : null}
    {groups.working.length ? <OverviewSection title="进行中" count={groups.working.length}>
      {groups.working.map((work) => <TaskOverviewCard key={work.work_id} work={work} bot={workspace.bots.find((item) => item.conversation_id === work.conversation_id)} lookup={lookup} readyDevice={ready?.entry.device_id} connected={workspace.connectionOnline} />)}
    </OverviewSection> : null}
    {groups.waiting.length ? <OverviewSection title="等待中" count={groups.waiting.length}>
      {groups.waiting.map((work) => <TaskOverviewCard key={work.work_id} work={work} bot={workspace.bots.find((item) => item.conversation_id === work.conversation_id)} lookup={lookup} readyDevice={ready?.entry.device_id} connected={workspace.connectionOnline} />)}
    </OverviewSection> : null}
    {!hasContent ? <EmptyState icon={ListChecks} title="没有未完成任务" description="Bot 开始工作后，任务会出现在这里。" /> : null}
  </div>
}

function taskGroup(work: OpenWork, nodes: NodeInfo[], waitingSessions: ReadonlySet<string>): TaskGroup {
  const state = shownState(work, nodes)
  return state === "unverified" || state === "awaiting_confirmation" || waitingSessions.has(work.thread_id ?? "") || !taskActive(state) ? "waiting" : "working"
}

function OverviewSection({ title, count, children }: { title: string; count: number; children: ReactNode }) {
  return <section className="grid gap-3" aria-labelledby={`tasks-${title}`}>
    <div className="flex items-center gap-2 px-1"><h2 id={`tasks-${title}`} className="text-heading font-semibold">{title}</h2><CountBadge count={count} label={`${count} 项`} /></div>
    <div className="grid gap-3">{children}</div>
  </section>
}

function TaskOverviewCard({ work, bot, lookup, readyDevice, connected }: { work: OpenWork; bot?: Pick<BotRecord, "bot_id" | "name">; lookup: { nodes: NodeInfo[]; directory: MembershipView | null }; readyDevice?: string; connected: boolean }) {
  const state = shownState(work, lookup.nodes)
  const stale = taskStale(state, connected)
  const label = stale ? staleTaskLabel : taskLabel(state)
  const tone = stale ? "neutral" : stateTone(state)
  const Icon = stale || state === "unverified" ? CircleQuestionMark : taskActive(state) ? LoaderCircle : ListChecks
  const DeviceIcon = work.target_node === readyDevice ? Laptop : Server
  const device = nodeName(work.target_node, lookup, "未知设备")
  const project = work.cwd ? projectName(work.cwd) : "未指定项目"
  const card = <Card className="min-w-0 transition-colors max-md:min-h-11" data-navigable={bot ? true : undefined}>
      <CardContent className="grid min-w-0 gap-3">
        <div className="flex min-w-0 items-start gap-3">
          <span className="grid size-9 shrink-0 place-items-center rounded-lg bg-muted text-muted-foreground"><Icon aria-hidden="true" className={taskActive(state) && !stale ? "size-5 animate-spin motion-reduce:animate-none text-info" : "size-5"} /></span>
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 flex-wrap items-start gap-2"><h3 className="min-w-0 flex-1 [overflow-wrap:anywhere] font-medium">{work.title || "任务"}</h3><StatusPill tone={tone}>{label}</StatusPill></div>
            <p className="mt-1 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-meta text-muted-foreground">
              <span className="inline-flex min-w-0 items-center gap-1.5"><Bot aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={bot?.name ?? "未知 Bot"}>{bot?.name ?? "未知 Bot"}</span></span>
              <span aria-hidden="true">·</span>
              <span className="inline-flex min-w-0 items-center gap-1.5"><DeviceIcon aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={device}>{device}</span></span>
              <span aria-hidden="true">·</span>
              <span className="inline-flex min-w-0 items-center gap-1.5"><FolderOpen aria-hidden="true" className="size-3.5 shrink-0" /><span className="truncate" title={work.cwd || "未指定项目"}>{project}</span></span>
            </p>
          </div>
          <ChevronRight aria-hidden="true" className="mt-1 size-4 shrink-0 text-muted-foreground" />
        </div>
        <p className="whitespace-pre-wrap break-words text-meta text-muted-foreground">{work.summary || work.prompt || "暂无进展"}</p>
      </CardContent>
    </Card>
  return bot ? <Link to="/b/$botId/work/$workId" params={{ botId: bot.bot_id, workId: work.work_id }} search={{ from: "tasks" }} className="group block min-w-0 rounded-xl outline-none focus-visible:ring-3 focus-visible:ring-ring/50">{card}</Link> : card
}
