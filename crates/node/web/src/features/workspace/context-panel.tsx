"use client"

import { useLayoutEffect, useRef } from "react"
import { BrainCircuit, ChevronLeft, CircleCheck, CircleDashed, CircleHelp, CircleQuestionMark, CircleX, Cpu, EyeOff, History, Laptop, ListChecks, MessageCircle, MessagesSquare, ScrollText, Server, Unplug, UserRound, type LucideIcon } from "lucide-react"
import { Link } from "@tanstack/react-router"
import { Button } from "@/components/ui/button"
import { DetailRow, Details, SettingsGroup, SettingsRow, StatusPill, type Tone } from "@/components/app/kit"
import { formatWhen, changeTitle } from "@vibeface/client/workspace/bot-settings.ts"
import { nodeName, shownState, taskLabel } from "@vibeface/client/workspace/labels.ts"
import { providerNames } from "@vibeface/client/lib/node.ts"
import type { BotChange, BotRecord, ConversationPayload, CoordinationCheck, NodeInfo, Question, SessionHandoff, WorkRecord } from "@vibeface/client/workspace/types.ts"
import { providerIcons } from "@/features/devices/ai-settings"
import { CollaborationPanel } from "./collaboration-panel"

type ContextPanelProps = {
  bot: Pick<BotRecord, "bot_id" | "name">
  data: ConversationPayload
  nodes: readonly NodeInfo[]
  onBack: () => void
}

const continuityProviderLabels: Record<string, string> = { codex: "Codex", cursor: "Cursor", claude: "Claude Code", pi: "pi", opencode: "opencode" }

/** A read-only view of the fields already present in one ConversationPayload. */
export function ContextPanel({ bot, data, nodes, onBack }: ContextPanelProps) {
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
        <li><span className="font-medium text-foreground">远端 owner：</span>负责设备和可达性来自快照里的远端归属。</li>
        <li><span className="font-medium text-foreground">不可见 checkpoint：</span>快照没有返回检查点摘要，不从聊天内容或原生会话猜测。</li>
      </ul>
    </Details>
    <AgentContinuity data={data} nodes={nodes} />

    <SettingsGroup title="Bot 身份">
      <SettingsRow icon={UserRound} label="Bot" description="当前聊天" children={<span className="max-w-44 truncate" title={bot.name}>{bot.name}</span>} />
      {owner ? <SettingsRow icon={owner === data.local_node ? Laptop : Server} label="负责设备" description="共享聊天事实" children={<span className="max-w-44 truncate" title={ownerName}>{ownerName}</span>} /> : null}
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
        <div className="flex flex-wrap items-center gap-2"><StatusPill tone="warning" icon={EyeOff}>检查点不可见</StatusPill><span className="text-meta text-muted-foreground">没有从聊天或本机会话猜测</span></div>
        <Details label="为什么看不到检查点"><p>当前快照没有共享检查点字段。这里不会补造完整上下文，也不会显示原生会话令牌或 Provider 原始输出。</p></Details>
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
      <CollaborationPanel data={data} botId={bot.bot_id} className="gap-4" showWork={false} showQuestions={false} />
    </section>

    <p className="px-1 text-meta text-muted-foreground">本面板只显示当前快照里的摘要。任务操作仍在任务详情中进行。</p>
  </div>
}

type ContinuityStatus = { tone: Tone; icon: LucideIcon; label: string; detail: string }

function AgentContinuity({ data, nodes }: { data: ConversationPayload; nodes: readonly NodeInfo[] }) {
  const sessions = [...data.sessions].sort((left, right) => right.created_at_ms - left.created_at_ms || left.session_id.localeCompare(right.session_id))
  const scheduler = data.scheduler
  const remote = scheduler?.remote
  const activeJob = scheduler?.jobs.some((job) => ["queued", "claimed", "running", "steering", "cancelling"].includes(job.state))
  const status: ContinuityStatus = remote?.reachable === false
    ? { tone: "warning", icon: Unplug, label: "远端状态未知", detail: "owner 不可达时，排队和回复结果不能视为已执行。" }
    : scheduler
      ? { tone: "info", icon: activeJob ? Cpu : CircleCheck, label: activeJob ? "正在运行" : "状态已读取", detail: `${providerLabel(scheduler.provider)} · ${scheduler.model || "设备默认模型"}` }
      : sessions.length
        ? { tone: "neutral", icon: History, label: "只有交接记录", detail: "当前没有运行状态。" }
        : { tone: "neutral", icon: CircleDashed, label: "尚未建立", detail: "Bot 身份和共享事实仍会保留。" }
  const owner = data.conversation?.owner
  const ownerName = nodeName(owner ?? undefined, { nodes })
  const ownerLabel = remote?.owner
    ? remote.reachable === false ? "远端 owner 不可达" : `远端 owner · ${nodeName(remote.owner, { nodes })}`
    : owner === data.local_node ? "本机 owner"
      : owner ? `最近已知 owner · ${ownerName}`
        : "owner 未知"
  return <SettingsGroup title="长期 Agent" footer="共享事实可同步并重建。模型会话只是续接线索，不是聊天记录或密钥。">
    <div className="grid gap-4 p-4">
      <div className="flex min-w-0 items-start gap-3">
        <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-brand-soft text-brand ring-1 ring-brand/20"><BrainCircuit aria-hidden="true" className="size-5" /></span>
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 flex-wrap items-center gap-2"><h3 className="min-w-0 flex-1 font-medium">持续身份与恢复线索</h3><StatusPill tone={status.tone} icon={status.icon}>{status.label}</StatusPill></div>
          <p className="mt-1 text-meta text-muted-foreground">{status.detail}</p>
        </div>
      </div>
      <ol className="grid gap-2 sm:grid-cols-3" aria-label="长期 Agent 的上下文层次">
        <ContinuityLayer step="1" label="共享事实" detail={`${data.messages.length} 条消息 · ${data.works.length} 项工作`} tone="success" />
        <ContinuityLayer step="2" label="会话交接" detail={`${sessions.length} 条续接记录`} tone={sessions.length ? "info" : "neutral"} />
        <ContinuityLayer step="3" label="模型运行" detail="随模型服务重启而变化" tone="warning" />
      </ol>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        <ContinuityStat label="持久记忆" value={data.bot_settings.memory.length} />
        <ContinuityStat label="定时任务" value={data.bot_settings.schedules.length} />
        <ContinuityStat label="交接记录" value={sessions.length} />
        <ContinuityStat label="队列观察" value={scheduler?.jobs.length ?? 0} />
      </div>
      <div className="flex min-w-0 flex-wrap items-center gap-2 rounded-lg bg-muted/45 px-3 py-2 text-meta">
        <span className="font-medium">{ownerLabel}</span>
        {remote?.reachable === false ? <StatusPill tone="warning" icon={Unplug}>状态未知</StatusPill> : owner ? <StatusPill tone="info">归属已知</StatusPill> : <StatusPill tone="neutral">等待归属</StatusPill>}
      </div>
      <Details label="为什么要把三层分开">
        <p>消息、工作、设置变更和问题是共享事实。Provider session 只帮助设备定位原生会话，不能替代共享历史；原生会话消失时，下一轮应从共享事实重新建立。</p>
      </Details>
      <section className="grid gap-2" aria-labelledby="context-handoffs">
        <div className="flex items-center gap-2"><h3 id="context-handoffs" className="text-meta font-medium text-muted-foreground">会话交接</h3><span className="tabular-nums text-meta text-muted-foreground">· {sessions.length}</span></div>
        {sessions.length ? <ol className="grid gap-2 border-s border-border/70 ps-4">{sessions.map((session) => <ContinuitySession key={`${session.node}:${session.session_id}`} session={session} nodes={nodes} localNode={data.local_node} />)}</ol>
          : <div className="rounded-lg border border-dashed px-3 py-3 text-meta text-muted-foreground">还没有会话交接。共享聊天事实仍然可见，首次模型运行后才会出现续接记录。</div>}
      </section>
    </div>
  </SettingsGroup>
}

function ContinuityLayer({ step, label, detail, tone }: { step: string; label: string; detail: string; tone: Tone }) {
  return <li className="grid min-w-0 gap-1 rounded-xl border border-border/70 bg-background/55 p-3">
    <div className="flex min-w-0 items-center gap-2"><span aria-hidden="true" className="grid size-5 shrink-0 place-items-center rounded-full bg-muted text-meta font-semibold tabular-nums">{step}</span><span className="min-w-0 truncate font-medium">{label}</span><StatusPill tone={tone}>{tone === "success" ? "可重建" : tone === "info" ? "续接线索" : "不共享"}</StatusPill></div>
    <p className="ps-7 text-meta text-muted-foreground">{detail}</p>
  </li>
}

function ContinuityStat({ label, value }: { label: string; value: number }) {
  return <div className="grid gap-0.5 rounded-xl border border-border/70 bg-background/55 px-3 py-2"><span className="text-meta text-muted-foreground">{label}</span><span className="text-lg font-semibold tabular-nums">{value}</span></div>
}

function ContinuitySession({ session, nodes, localNode }: { session: SessionHandoff; nodes: readonly NodeInfo[]; localNode: string }) {
  const Icon = providerIcons[session.agent]
  const node = nodes.find((item) => item.device_id === session.node)
  const status: ContinuityStatus = session.muted
    ? { tone: "neutral", icon: CircleDashed, label: "已静音", detail: "这条交接记录仍保留，但当前 Bot 不再关注它。" }
          : node?.revoked
      ? { tone: "danger", icon: CircleX, label: "设备已撤权", detail: "不能把这条记录当作当前可用的模型会话。" }
      : !node
        ? { tone: "neutral", icon: CircleDashed, label: "可达性未知", detail: "快照没有这台设备的状态。" }
        : !node.online
          ? { tone: "warning", icon: Unplug, label: "设备不可达", detail: "设备恢复前，续接和结果都不能确认。" }
          : { tone: "info", icon: node.local || session.node === localNode ? Laptop : Server, label: node.local || session.node === localNode ? "本机线索" : "设备可达", detail: "设备可达不等于原生 session 仍在运行。" }
  return <li className="relative grid min-w-0 gap-2 rounded-xl border bg-card p-3 shadow-xs">
    <div className="flex min-w-0 items-start gap-2.5">
      <span className="grid size-7 shrink-0 place-items-center rounded-lg bg-muted text-muted-foreground"><Icon aria-hidden="true" className="size-4" /></span>
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-center gap-2"><span className="min-w-0 flex-1 truncate font-medium">接手会话 · {providerNames[session.agent]}</span><StatusPill tone={status.tone} icon={status.icon}>{status.label}</StatusPill></div>
        <p className="mt-1 min-w-0 break-words text-meta text-muted-foreground">{nodeName(session.node, { nodes })} · {session.cwd || "未记录项目"}</p>
      </div>
      <time className="shrink-0 text-meta text-muted-foreground tabular-nums max-sm:order-last max-sm:w-full max-sm:ps-9" dateTime={new Date(session.created_at_ms).toISOString()}>{formatWhen(session.created_at_ms)}</time>
    </div>
    {session.summary ? <p className="ps-9 line-clamp-3 whitespace-pre-wrap break-words text-meta text-muted-foreground">{session.summary}</p> : null}
    <p className="ps-9 text-meta text-muted-foreground">{status.detail}</p>
    <Details label="查看会话边界" className="ps-9">
      <DetailRow label="Provider session ID" value={session.session_id} mono />
      <DetailRow label="项目目录" value={session.cwd || "未记录"} mono />
      <p>这个 ID 只用于续接定位，不是共享聊天历史，也不是 Provider token。</p>
    </Details>
  </li>
}

function providerLabel(provider?: string) {
  if (!provider) return "设备默认模型"
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
