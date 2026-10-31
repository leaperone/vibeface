"use client"

import { useLayoutEffect, useRef } from "react"
import { BrainCircuit, ChevronLeft, CircleCheck, CircleDashed, CircleHelp, CircleQuestionMark, CircleX, Cpu, EyeOff, Laptop, ListChecks, MessageCircle, MessagesSquare, ScrollText, Unplug, UserRound, type LucideIcon } from "lucide-react"
import { Link } from "@tanstack/react-router"
import { Button } from "@/components/ui/button"
import { Details, SettingsGroup, SettingsRow, StatusPill, type Tone } from "@/components/app/kit"
import { formatWhen, changeTitle, scheduleFrequency } from "@vibeface/client/workspace/bot-settings.ts"
import { contextCoverageLabels, contextIndexLabels, contextInvalidation, contextStages, memoryConflictLabel, memoryOrigins, nativeSessionObservation, nodeName, schedulerStatus, shownState, taskLabel } from "@vibeface/client/workspace/labels.ts"
import type { BotChange, BotRecord, BotSettings, ContextProjection, ConversationPayload, CoordinationCheck, NodeInfo, Question, WorkRecord } from "@vibeface/client/workspace/types.ts"
import { CollaborationPanel } from "./collaboration-panel"
import { cn } from "@/lib/utils"

type ContextPanelProps = {
  bot: Pick<BotRecord, "bot_id" | "name">
  data: ConversationPayload
  nodes: readonly NodeInfo[]
  connected: boolean
  onBack: () => void
}

const continuityProviderLabels: Record<string, string> = { codex: "Codex", cursor: "Cursor", claude: "Claude Code", pi: "pi", opencode: "opencode" }

/** A read-only view of the fields already present in one ConversationPayload. */
export function ContextPanel({ bot, data, nodes, connected, onBack }: ContextPanelProps) {
  const root = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    root.current?.closest<HTMLElement>('[data-slot="sheet-content"]')?.scrollTo({ top: 0, behavior: "auto" })
  }, [])
  const owner = data.conversation?.owner
  const ownerName = nodeName(owner ?? undefined, { nodes })
  const messages = data.messages.slice(-4)
  const works = data.works
  const changes = data.changes
  const questions = data.questions
  const checks = data.checks
  return <div ref={root} className="grid gap-5 px-4 pb-[max(1.5rem,env(safe-area-inset-bottom))]">
    <header className="flex min-w-0 items-start gap-2">
      <Button type="button" variant="ghost" size="icon" className="-ms-2 shrink-0" aria-label="返回 Bot 设置" title="返回 Bot 设置" onClick={onBack}><ChevronLeft aria-hidden="true" /></Button>
      <div className="min-w-0 flex-1 pt-1">
        <h2 className="text-heading font-semibold">上下文和会话</h2>
        <p className="truncate text-meta text-muted-foreground">只读摘要，随当前聊天更新</p>
      </div>
      <StatusPill tone="info" icon={MessagesSquare}>共享事实</StatusPill>
    </header>
    <section className="relative overflow-hidden rounded-2xl border border-brand/15 bg-linear-to-br from-brand-soft/55 via-card to-card p-4 shadow-sm" aria-label="上下文概览">
      <div aria-hidden="true" className="pointer-events-none absolute -end-10 -top-12 size-32 rounded-full bg-brand/10 blur-2xl" />
      <div className="relative grid gap-4">
        <div className="flex min-w-0 items-center gap-3"><span className="grid size-9 shrink-0 place-items-center rounded-xl bg-brand-soft text-brand ring-1 ring-brand/20"><MessagesSquare aria-hidden="true" className="size-5" /></span><div className="min-w-0"><p className="font-medium">共享上下文</p><p className="text-meta text-muted-foreground">事实、工作和会话各自有边界</p></div></div>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <ContextStat label="消息" value={data.messages.length} />
          <ContextStat label="工作" value={works.length} />
          <ContextStat label="问题" value={questions.length} />
          <ContextStat label="检查" value={checks.length} />
        </div>
      </div>
    </section>
    <Details label="数据来源和边界">
      <ul className="grid gap-2">
        <li><span className="font-medium text-foreground">共享聊天事实：</span>消息、工作、变更和问题来自当前聊天快照。</li>
        <li><span className="font-medium text-foreground">本机调度观察：</span>排队和运行状态只代表这台设备最近一次看到的情况。</li>
        <li><span className="font-medium text-foreground">负责设备：</span>其他设备负责时，运行状态来自它最近返回的观察。</li>
        <li><span className="font-medium text-foreground">编译摘要：</span>只包含版本指纹和覆盖数量，不包含完整提示词或 AI 原生会话。</li>
      </ul>
    </Details>
    <AgentContinuity data={data} nodes={nodes} connected={connected} />

    <SettingsGroup title="Bot 身份">
      <SettingsRow icon={UserRound} label="Bot" description="当前聊天" children={<span className="max-w-44 truncate" title={bot.name}>{bot.name}</span>} />
      {owner ? <SettingsRow icon={Laptop} label="负责设备" description="共享聊天事实" children={<span className="max-w-44 truncate" title={ownerName}>{ownerName}</span>} /> : null}
    </SettingsGroup>

    <SettingsGroup title="共享上下文">
      <SettingsRow icon={MessageCircle} label="消息摘要" description="共享聊天事实" children={<span>{data.messages.length} 条</span>} />
      {messages.length ? <Details label="查看最近消息">
        <ul className="grid gap-3">
          {messages.map((message) => <li key={message.message_id} className="grid min-w-0 gap-1 rounded-lg border bg-card px-3 py-2">
            <div className="flex min-w-0 items-center gap-2 text-meta">
              <span className="font-medium">{message.sender?.name ?? roleName(message.role)}</span>
              <time className="ms-auto shrink-0 text-muted-foreground tabular-nums" dateTime={new Date(message.created_at_ms).toISOString()}>{formatWhen(message.created_at_ms)}</time>
            </div>
            <p className="line-clamp-3 whitespace-pre-wrap [overflow-wrap:anywhere]">{message.text}</p>
          </li>)}
        </ul>
      </Details> : <p className="px-4 pb-3 text-meta text-muted-foreground">还没有聊天记录</p>}
      <SettingsRow icon={ListChecks} label="工作摘要" description="共享工作投影" children={<span>{works.length} 项</span>} />
      {works.length ? <Details label="查看工作摘要">
        <ul className="grid gap-2">
          {works.map((work) => <WorkSummary key={work.work_id} work={work} botId={bot.bot_id} nodes={nodes} />)}
        </ul>
      </Details> : <p className="px-4 pb-3 text-meta text-muted-foreground">还没有工作</p>}
      <div className="grid gap-2 px-4 py-3">
        <div className="flex flex-wrap items-center gap-2"><StatusPill tone="warning" icon={EyeOff}>完整历史摘要未提供</StatusPill><span className="text-meta text-muted-foreground">不从聊天或本机会话猜测</span></div>
        <Details label="为什么没有完整历史摘要"><p>当前快照只提供本轮上下文的覆盖数量，不返回完整历史或交接内容。已编译不代表已发送，AI 会话缓存也不是聊天记录。</p></Details>
      </div>
    </SettingsGroup>

    <SettingsGroup title={`设置变更 · ${changes.length}`}>
      <SettingsRow icon={ScrollText} label="变更记录" description="共享聊天事实" children={<span>{changes.length} 条</span>} />
      {changes.length ? <Details label="查看变更记录">
        <ul className="grid gap-2">
          {changes.map((record) => <li key={record.change_id} className="grid min-w-0 gap-1 rounded-lg border bg-card px-3 py-2">
            <div className="flex min-w-0 items-center gap-2"><span className="min-w-0 truncate font-medium">{changeTitle([record.change])}</span><time className="ms-auto shrink-0 text-meta text-muted-foreground tabular-nums" dateTime={new Date(record.created_at_ms).toISOString()}>{formatWhen(record.created_at_ms)}</time></div>
            <p className="text-meta text-muted-foreground">{changeFields(record.change)}</p>
          </li>)}
        </ul>
      </Details> : null}
    </SettingsGroup>

    <SettingsGroup title="问题和检查">
      <SettingsRow icon={CircleHelp} label="问题" description="共享聊天事实" children={<span>{questions.length} 条</span>} />
      {questions.length ? <Details label="查看问题">
        <ul className="grid gap-2">{questions.map((question) => <QuestionSummary key={question.question_id} question={question} />)}</ul>
      </Details> : null}
      <SettingsRow icon={CircleQuestionMark} label="检查" description="本机观察" children={<span>{checks.length} 项</span>} />
      {checks.length ? <Details label="查看检查">
        <ul className="grid gap-2">{checks.map((check) => <CheckSummary key={check.check_id} check={check} />)}</ul>
      </Details> : null}
    </SettingsGroup>

    <section className="grid gap-3" aria-label="协作与队列">
      <h3 className="px-1 text-meta font-medium text-muted-foreground">协作与队列</h3>
      <CollaborationPanel data={data} botId={bot.bot_id} className="gap-4" />
    </section>

    <p className="px-1 text-meta text-muted-foreground">本面板只显示当前快照里的摘要。任务操作仍在任务详情中进行。</p>
  </div>
}

type ContinuityStatus = { tone: Tone; icon: LucideIcon; label: string; detail: string }

function AgentContinuity({ data, nodes, connected }: { data: ConversationPayload; nodes: readonly NodeInfo[]; connected: boolean }) {
  const scheduler = data.scheduler
  const remote = scheduler?.remote
  const context = scheduler?.context
  const stage = context && contextStages[context.stage]
  const unavailable = !connected || remote?.reachable === false
  const native = unavailable ? null : context?.session_status
  const nativeStatus = nativeSessionObservation(native)
  const openJobs = scheduler?.jobs.filter((job) => schedulerStatus(job.state).group !== "history") ?? []
  const status: ContinuityStatus = unavailable
    ? { tone: "warning", icon: Unplug, label: "当前状态未知", detail: "连接中断或负责设备不可达，显示最近读取的记录。" }
    : scheduler
      ? { tone: "info", icon: openJobs.length ? Cpu : CircleCheck, label: openJobs.length === 1 ? schedulerStatus(openJobs[0].state).label : openJobs.length ? `${openJobs.length} 项后台工作未结束` : "状态已读取", detail: [scheduler.provider ? providerLabel(scheduler.provider) : null, scheduler.model || "设备默认模型"].filter(Boolean).join(" · ") }
      : { tone: "neutral", icon: CircleDashed, label: "后台工作状态未提供", detail: "Bot 身份和共享事实仍会保留。" }
  const owner = data.conversation?.owner
  const ownerName = nodeName(owner ?? undefined, { nodes })
  const ownerLabel = remote?.owner
    ? remote.reachable === false ? "负责设备不可达" : `负责设备 · ${nodeName(remote.owner, { nodes })}`
    : owner === data.local_node ? "由本机负责"
      : owner ? `最近已知负责设备 · ${ownerName}`
        : "负责设备未知"
  return <SettingsGroup title="长期 Agent" footer="聊天事实和记忆保存于共享记录。AI 会话只是本机执行缓存。">
    <div className="grid gap-4 p-4">
      <div className="flex min-w-0 items-start gap-3">
        <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-brand-soft text-brand ring-1 ring-brand/20"><BrainCircuit aria-hidden="true" className="size-5" /></span>
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 flex-wrap items-center gap-2"><h3 className="min-w-0 flex-1 font-medium">持续身份与后台工作</h3><StatusPill tone={status.tone} icon={status.icon}>{status.label}</StatusPill></div>
          <p className="mt-1 text-meta text-muted-foreground">{status.detail}</p>
        </div>
      </div>
      <ol className="grid gap-2 sm:grid-cols-3" aria-label="长期 Agent 的上下文层次">
        <ContinuityLayer step="1" label="共享事实" detail={`${data.messages.length} 条消息 · ${data.works.length} 项工作`} tone="success" status="已保存" />
        <ContinuityLayer step="2" label="外部会话" detail={`${data.sessions.length} 条归属记录`} tone="info" status="外部归属" />
        <ContinuityLayer step="3" label="Bot 自身会话" detail={nativeStatus.detail} tone={native?.resumable && !native.pending_generation ? 'info' : 'warning'} status={nativeStatus.label} />
      </ol>
      <div className="grid gap-2">
        <div className="flex flex-wrap items-center gap-2"><span className="font-medium">本轮上下文</span><StatusPill tone={unavailable ? 'warning' : stage?.tone ?? 'neutral'}>{unavailable ? '当前状态未知' : stage?.label ?? '未提供'}</StatusPill></div>
        <p className="text-meta text-muted-foreground">{unavailable ? '下面保留最近读取的编译记录，当前执行和续接状态未知。' : stage?.detail ?? '当前记录未提供编译或发送状态。'}</p>
        {context ? <ContextDetails context={context} /> : null}
      </div>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        <ContinuityStat label="持久记忆" value={data.bot_settings.memory.length} />
        <ContinuityStat label="定时任务" value={data.bot_settings.schedules.length} />
        <ContinuityStat label="外部归属" value={data.sessions.length} />
        <ContinuityStat label="后台工作" value={scheduler?.jobs.length ?? null} />
      </div>
      <MemorySummary settings={data.bot_settings} context={context} />
      {data.bot_settings.schedules.length ? <Details label={`查看定时任务 · ${data.bot_settings.schedules.length}`}>
        <ul className="grid gap-2">{data.bot_settings.schedules.map((schedule) => <li key={schedule.schedule_id} className={cn("grid gap-1 rounded-lg border bg-card px-3 py-2", !schedule.enabled && "text-muted-foreground")}>
          <div className="flex min-w-0 items-center gap-2"><span className="min-w-0 flex-1 truncate font-medium" title={schedule.name}>{schedule.name}</span><StatusPill tone={schedule.enabled ? "info" : "neutral"}>{schedule.enabled ? "启用" : "已暂停"}</StatusPill></div>
          <p className="line-clamp-2 whitespace-pre-wrap text-meta text-muted-foreground">{schedule.prompt}</p>
          <p className="text-meta text-muted-foreground">{scheduleFrequency(schedule)}{schedule.next_run_at_ms === null ? " · 尚未安排下一次运行" : ` · 下次 ${formatWhen(schedule.next_run_at_ms)}`}</p>
        </li>)}</ul>
      </Details> : <p className="text-meta text-muted-foreground">暂无待执行定时任务</p>}
      <div className="flex min-w-0 flex-wrap items-center gap-2 rounded-lg bg-muted/45 px-3 py-2 text-meta">
        <span className="font-medium">{ownerLabel}</span>
        {remote?.reachable === false ? <StatusPill tone="warning" icon={Unplug}>状态未知</StatusPill> : owner ? <StatusPill tone="info">归属已知</StatusPill> : <StatusPill tone="neutral">等待归属</StatusPill>}
      </div>
      <Details label="为什么要把三层分开">
        <p>消息、工作、设置变更和问题是共享事实。外部会话归属记录说明 Bot 关注了哪个外部会话。Bot 的本机缓存能否续接由负责设备报告，结果未知时需要先核对，不能重发操作。</p>
      </Details>
    </div>
  </SettingsGroup>
}

function ContextDetails({ context }: { context: ContextProjection }) {
  return <Details label="本轮上下文详情">
    <p>会话代次 {context.session_generation} · {contextStages[context.stage].label} · {providerLabel(context.provider)}</p>
    {context.invalidation_reason ? <p>{contextInvalidation(context.invalidation_reason)}，本代次使用新的上下文。</p> : null}
    <p>{context.session_status?.pending_generation ? '配置已更新，当前回合继续使用原配置，下一轮生效。' : '配置变更从下一轮生效，当前回合按启动时的配置继续。'}</p>
    <p>以下为本轮编译时因预算省略的数量。省略内容仍保存在共享记录，可按需读取。</p>
    <ul className="grid grid-cols-2 gap-x-3 gap-y-1">{Object.entries(context.coverage).map(([key, count]) => typeof count === 'number' ? <li key={key}>{contextCoverageLabels[key] ?? '其他内容'} · {count}</li> : null)}{Object.entries(context.coverage.bot_indexes_omitted).map(([key, count]) => <li key={`index:${key}`}>{contextIndexLabels[key] ?? '其他索引'} · {count}</li>)}</ul>
    <p className="text-meta">稳定内容 {context.stable_bytes} 字节 · 本轮内容 {context.dynamic_bytes} 字节</p>
    <p className="break-all font-mono text-meta">稳定指纹 {context.stable_fingerprint}</p>
    <p className="break-all font-mono text-meta">本轮指纹 {context.dynamic_fingerprint}</p>
  </Details>
}

export function MemorySummary({ settings, context }: { settings: BotSettings; context?: ContextProjection | null }) {
  return <div className="grid gap-3">
    <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
      <ContinuityStat label="已保存记忆" value={settings.memory.length} />
      <ContinuityStat label="待提交" value={settings.memory_staged_count ?? null} />
      <ContinuityStat label="提交冲突" value={settings.memory_conflicts?.length ?? null} />
      <ContinuityStat label="本轮省略" value={context?.coverage.memory_omitted ?? null} />
    </div>
    <p className="text-meta text-muted-foreground">待提交和冲突来自负责设备最近的观察。待提交记忆只在本轮成功后保存；失败、中断或结果未知时丢弃。省略的记忆仍可按需读取。</p>
    {settings.memory.length ? <Details label={`查看已保存记忆 · ${settings.memory.length}`}>
      <ul className="grid gap-2">{settings.memory.map((entry) => <li key={entry.memory_id} className="grid gap-2 rounded-lg border bg-card px-3 py-2">
        <p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{entry.text}</p>
        {entry.metadata ? <>
          <p className="text-meta text-muted-foreground">{memoryOrigins[entry.metadata.origin]} · {entry.metadata.user_controlled ? '由你维护，受保护' : 'Bot 可更新'} · {entry.metadata.essential ? '优先带入上下文' : '按需读取'}</p>
          <Details label="记忆版本"><p className="break-all font-mono text-meta">数据版本 {entry.metadata.epoch} · {entry.metadata.revision}</p>{entry.metadata.source_event_hash ? <p className="break-all font-mono text-meta">来源事件 {entry.metadata.source_event_hash}</p> : null}</Details>
        </> : <p className="text-meta text-muted-foreground">来源与版本未提供</p>}
      </li>)}</ul>
    </Details> : <p className="text-meta text-muted-foreground">暂无已保存记忆</p>}
    {settings.memory_conflicts?.length ? <Details label={`查看记忆冲突 · ${settings.memory_conflicts.length}`}>
      <ul className="grid gap-2">{settings.memory_conflicts.map((conflict) => <li key={`${conflict.call_ref}:${conflict.memory_id}`} className="grid gap-1"><p>{memoryConflictLabel(conflict.reason)}</p><p className="break-all font-mono text-meta">记忆 {conflict.memory_id} · 数据版本 {conflict.expected_epoch} / {conflict.actual_epoch}</p><p className="break-all font-mono text-meta">预期 {conflict.expected_revision || '尚无版本'} · 当前 {conflict.actual_revision || '已移除'}</p></li>)}</ul>
    </Details> : null}
  </div>
}

function ContinuityLayer({ step, label, detail, tone, status }: { step: string; label: string; detail: string; tone: Tone; status: string }) {
  return <li className="grid min-w-0 gap-1 rounded-xl border border-border/70 bg-background/55 p-3">
    <div className="flex min-w-0 items-center gap-2"><span aria-hidden="true" className="grid size-5 shrink-0 place-items-center rounded-full bg-muted text-meta font-semibold tabular-nums">{step}</span><span className="min-w-0 truncate font-medium">{label}</span><StatusPill tone={tone}>{status}</StatusPill></div>
    <p className="ps-7 text-meta text-muted-foreground">{detail}</p>
  </li>
}

function ContinuityStat({ label, value }: { label: string; value: number | null }) {
  return <div className="grid gap-0.5 rounded-xl border border-border/70 bg-background/55 px-3 py-2"><span className="text-meta text-muted-foreground">{label}</span><span className="text-lg font-semibold tabular-nums">{value ?? "未提供"}</span></div>
}

function providerLabel(provider: string) {
  return continuityProviderLabels[provider] ?? provider
}

function ContextStat({ label, value }: { label: string; value: number }) {
  return <div className="grid gap-0.5 rounded-xl border border-border/70 bg-background/55 px-3 py-2"><span className="text-meta text-muted-foreground">{label}</span><span className="text-lg font-semibold tabular-nums">{value}</span></div>
}

function roleName(role: string) {
  if (role === "user") return "你"
  if (role === "bot") return "另一个 Bot"
  return "Bot"
}

function WorkSummary({ work, botId, nodes }: { work: WorkRecord; botId: string; nodes: readonly NodeInfo[] }) {
  const state = shownState(work, [...nodes])
  return <li className="min-w-0 rounded-lg border bg-card px-3 py-2">
    <Link to="/b/$botId/work/$workId" params={{ botId, workId: work.work_id }} className="flex min-w-0 items-start gap-2 rounded-md outline-none focus-visible:ring-3 focus-visible:ring-ring/50">
      <StatusPill tone={workTone(state)} icon={workIcon(state)}>{taskLabel(state)}</StatusPill>
      <span className="min-w-0 flex-1"><span className="block truncate font-medium" title={work.title}>{work.title || "未命名工作"}</span>{work.summary ? <span className="block line-clamp-2 text-meta text-muted-foreground [overflow-wrap:anywhere]">{work.summary}</span> : null}</span>
    </Link>
  </li>
}

function workTone(state: string): Tone {
  if (["completed", "finished"].includes(state)) return "success"
  if (["failed", "rejected"].includes(state)) return "danger"
  if (["unverified", "awaiting_confirmation", "dispatch_unverified", "unknown"].includes(state)) return "warning"
  return "info"
}

function workIcon(state: string): LucideIcon {
  if (["completed", "finished"].includes(state)) return CircleCheck
  if (["failed", "rejected"].includes(state)) return CircleX
  if (["unverified", "awaiting_confirmation", "dispatch_unverified", "unknown"].includes(state)) return CircleQuestionMark
  return ListChecks
}

function changeFields(change: BotChange): string {
  if (change.kind === "profile") return Object.keys(change).filter((field) => field !== "kind").map((field) => profileField(field)).join("、") || "资料变化"
  if (change.kind === "skill") return `Skill · ${change.after?.name ?? change.before?.name ?? "已移除"}`
  if (change.kind === "schedule") return `定时任务 · ${change.after?.name ?? change.before?.name ?? "已移除"}`
  return `笔记 · ${change.after ?? change.before ?? "已移除"}`
}

function profileField(field: string) {
  return ({ name: "名称", instructions: "说明", model: "模型", skills: "Skill", avatar: "长相" } as Record<string, string>)[field] ?? field
}

function QuestionSummary({ question }: { question: Question }) {
  const settled = question.answer ? "已回答" : question.closed ? "已失效" : "等待回答"
  return <li className="grid min-w-0 gap-1 rounded-lg border bg-card px-3 py-2">
    <div className="flex min-w-0 items-center gap-2"><StatusPill tone={question.answer ? "success" : question.closed ? "neutral" : "warning"} icon={question.answer ? CircleCheck : question.closed ? CircleX : CircleHelp}>{settled}</StatusPill><span className="min-w-0 truncate font-medium" title={question.text}>{question.text}</span></div>
    {question.answer ? <p className="line-clamp-2 text-meta text-muted-foreground [overflow-wrap:anywhere]">{question.answer.text}</p> : null}
  </li>
}

function CheckSummary({ check }: { check: CoordinationCheck }) {
  return <li className="grid min-w-0 gap-1 rounded-lg border bg-card px-3 py-2">
    <div className="flex min-w-0 items-center gap-2"><StatusPill tone="warning" icon={CircleQuestionMark}>需要查看</StatusPill><span className="min-w-0 truncate font-medium" title={check.trigger}>{check.trigger}</span></div>
    {check.detail ? <p className="line-clamp-2 text-meta text-muted-foreground [overflow-wrap:anywhere]">{check.detail}</p> : null}
  </li>
}

export type { ContextPanelProps }
