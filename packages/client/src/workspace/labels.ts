import { asProvider, type MembershipView, type ProviderId } from "../lib/node.ts"
import type { LiveProgress } from "../lib/progress.ts"
import { confirmReason } from "./model.ts"
import type { NodeInfo, OverviewCoverage, SessionHandoff, WorkRecord } from "./types.ts"

const stateNames: Record<string, string> = {
  accepted: "已接收",
  pending_delivery: "等待设备接收",
  queued: "等待处理",
  steering: "Bot 正在看这条补充",
  steered: "已补充给 Bot",
  claimed: "正在准备",
  running: "执行中",
  waiting: "需要你同意",
  cancelling: "正在确认是否已取消",
  interrupting: "正在确认是否已停止",
  interrupted: "已中断",
  starting: "正在启动",
  unverified: "待确认",
  stopped: "已停止",
  unknown: "结果未知",
  pending_check: "待确认",
  succeeded: "操作完成",
  failed: "失败",
  rejected: "已拒绝",
  open: "已打开",
  opening: "正在打开",
  working: "处理中",
  needs_review: "待验收",
  idle: "空闲",
  pending: "待处理",
  completed: "已完成",
  awaiting_acceptance: "待验收",
  cancelled: "已取消",
  blocked: "受阻",
  ready: "就绪",
  awaiting_input: "待处理",
  finishing: "收尾中",
}

type NodeNameLookup = {
  nodes?: readonly NodeInfo[]
  directory?: MembershipView | null
}

export function nodeName(
  id: string | undefined,
  lookup?: NodeNameLookup,
  fallback?: string,
) {
  if (!id) return "未知设备"
  const node = lookup?.nodes?.find((item) => item.device_id === id)
  if (node?.name) return node.name
  const member = lookup?.directory?.members.find((item) => item.device_id === id)
  if (member?.name) return member.name
  if (node?.local) return "本机"
  return fallback ?? "另一台设备"
}

export function isLoopback(address: string) {
  try { return ["localhost", "127.0.0.1", "[::1]"].includes(new URL(address).hostname) } catch { return false }
}

/** Short display name for an access address. */
export function addressName(address: string) {
  if (isLoopback(address)) return "这台电脑"
  try { return new URL(address).hostname } catch { return address }
}

/**
 * The connected computer's name for the connection capsule. A member reads it from the directory; until the directory
 * lists that computer it is `null`, since the computer's own name may predate its join. Before joining, its own name.
 */
export function entryLabel(input: { joined: boolean; directory: Pick<MembershipView, "members"> | null | undefined; entryId: string | undefined; ownName: string | undefined; address: string }): string | null {
  const member = input.directory?.members.find((item) => item.device_id === input.entryId)?.name
  if (member) return member
  return input.joined ? null : input.ownName || addressName(input.address)
}

/** Joins and members carry only node or client, so a client named like a tablet or a phone counts as one, everywhere it shows. */
export type DeviceForm = "computer" | "tablet" | "phone" | "browser"
export const deviceForm = (kind: "node" | "client", name: string): DeviceForm =>
  kind === "node" ? "computer" : /ipad|tablet|平板/i.test(name) ? "tablet" : /iphone|android|phone|手机/i.test(name) ? "phone" : "browser"
export const formName: Record<DeviceForm, string> = { computer: "电脑", tablet: "平板", phone: "手机", browser: "浏览器" }

/**
 * A computer's reachability as the Node reports it; `entry` marks the computer this device connects through, online
 * before that read. Another browser or phone has none: only the Node it happens to be connected to would know; `self`,
 * the device in hand, is online while its connection is. While this device is offline the last report is stale: the
 * computer it lost reads offline, the others unknown.
 */
export const deviceOnline = (kind: "node" | "client", node: Pick<NodeInfo, "online"> | undefined, entry: boolean, connected: boolean, self = false) =>
  kind !== "node" ? (self ? connected : undefined) : !connected ? (entry ? false : undefined) : node?.online ?? entry
export const hostOf = (address: string) => { try { return new URL(address).host } catch { return "" } }

const taskStates: Record<string, string> = {
  pending_delivery: "等待中", queued: "等待中", claimed: "准备中", accepted: "准备中", running: "运行中", waiting: "需要你同意",
  cancelling: "停止中", interrupting: "停止中", completed: "已完成", failed: "失败", cancelled: "已停止", interrupted: "已中断",
  pending_check: "待确认", unknown: "待确认", starting: "准备中", finished: "已完成", unverified: "待确认", awaiting_confirmation: "待确认",
  dispatch_waiting: "等设备接收", dispatch_rejected: "未接收", awaiting_children: "等子任务", dispatch_unverified: "结果未知", accepted_waiting_sync: "等待同步", stop_waiting: "停止中", stop_timed_out: "还在尝试停止",
}
export const taskLabel = (state: string) => taskStates[state] ?? stateNames[state] ?? state
/** A task still running when this device went offline may have moved on since; it reads unknown, not running. */
export const taskStale = (state: string, connected: boolean) => !connected && taskActive(state)
export const staleTaskLabel = "状态未知"
/** A task that stopped without a cause of its own: stopped by hand, or its device or app went away mid-turn. Nothing redoes it. */
export const interruptedCopy = { reason: "中途停下了，不会自动重做" }
/** A task sent to a device that is offline starts once that device is back. */
export const taskCopy = { away: "离线，上线后开始" }
/** What the person asks the bot after a task ended badly, in both apps. */
export type FollowUp = "check" | "resume" | "retry"
export const followUpCopy: Record<FollowUp, { action: string; ask: (title: string) => string }> = {
  check: { action: "让 Bot 查结果", ask: (title) => `看看「${title}」做完了没有，先不要重做` },
  resume: { action: "让 Bot 接着做", ask: (title) => `接着做「${title}」` },
  retry: { action: "让 Bot 再试", ask: (title) => `再试一次「${title}」` },
}
/** An unknown outcome may have done its writes, so the bot checks it rather than running it again; a stopped task goes on. */
export const followUp = (state: string, unknown: boolean): FollowUp => unknown ? "check" : state === "interrupted" ? "resume" : "retry"

// Live progress reports provider words such as "running" or "awaiting_input"; anything unknown reads as plain progress.
const progressStates: Record<string, string> = {
  running: "进行中", in_progress: "进行中", finishing: "收尾中", completed: "已完成", failed: "失败", interrupted: "已中断",
  unknown: "结果未知", awaiting_input: "待处理", declined: "已拒绝",
}
export const progressLabel = (status: string) => progressStates[status] ?? (/[\u4e00-\u9fff]/.test(status) ? status : "进行中")

/**
 * Who runs a task, the same on every card: a provider names itself; an agent task runs as its device's bot turn, so
 * it names the provider that bot runs on there.
 */
export const taskAgent = (work: Pick<WorkRecord, "kind" | "target_node">, nodes: readonly NodeInfo[]): ProviderId | null =>
  work.kind === "task" ? asProvider(nodes.find((node) => node.device_id === work.target_node)?.capabilities?.manager) : work.kind ?? null
/** A task's newest step: what it recorded, else the last line or status its device streams, as another device's bot records none. */
export function taskProgress(work: Pick<WorkRecord, "work_id" | "target_node" | "summary">, live: readonly LiveProgress[]) {
  if (work.summary) return work.summary
  const stream = live.find((item) => item.kind === "native" && item.work_id === work.work_id && item.target_node === work.target_node)
  const line = stream?.text.trim().split("\n").at(-1) ?? ""
  if (line) return line
  const status = stream?.status ? progressLabel(stream.status) : ""
  return status === "进行中" ? "" : status
}

/**
 * A work only the person can settle now reads as awaiting confirmation; a finished one as its outcome; a stop not yet
 * confirmed as that stop, which keeps being retried.
 */
export const shownState = (work: WorkRecord, nodes: NodeInfo[] = []) =>
  confirmReason(work, nodes) ? "awaiting_confirmation" : work.outcome ?? work.stop ?? (work.state === "finished" ? "unknown" : work.state)
export const taskActive = (state: string) => ["accepted", "running", "interrupting", "dispatch_waiting", "stop_waiting", "stop_timed_out"].includes(state)
/**
 * A chat task's state: one that completed while a work it dispatched, or that work's own, still runs waits on them. No record
 * says so, so this reads only the works this device holds; a child not synced yet does not hold its parent.
 */
export function chatTaskState(work: WorkRecord, works: readonly WorkRecord[], nodes: NodeInfo[] = []): string {
  const state = shownState(work, nodes)
  if (state !== "completed") return state
  const seen = new Set([work.work_id])
  const open = (id: string): boolean => works.some((child) => {
    if (child.parent !== id || seen.has(child.work_id)) return false
    seen.add(child.work_id)
    const shown = shownState(child, nodes)
    return taskActive(shown) || shown === "awaiting_confirmation" || open(child.work_id)
  })
  return open(work.work_id) ? "awaiting_children" : state
}

/** A failed work, or a dispatch its device refused: the person sees it until something follows it up. */
export const failedWork = (work: Pick<WorkRecord, "outcome" | "state">) => work.outcome === "failed" || work.outcome === "rejected" || work.state === "dispatch_rejected"
/** A failure fixed in its device's settings: no bot there to take it, or the AI service there not set up. */
export const fixOnDevice = (work: Pick<WorkRecord, "failure">) =>
  ["missing_key", "auth_failed", "missing_binary", "execution_disabled", "no_model", "no_bot"].includes(work.failure ?? "")
/**
 * What a task was asked, short enough for its page: the first plain line, cut at 80 characters. Machine text such as JSON gives none,
 * and neither does a line that only repeats the title. `folded`: the prompt says more than the summary, so its text waits in 详细信息.
 */
export function promptSummary(prompt: string, title: string): { summary: string; folded: boolean } {
  const text = prompt.trim()
  const first = /^[{[]/.test(text) ? "" : text.split(/\r?\n/).map((line) => line.replace(/^#+\s*/, "").trim()).find(Boolean) ?? ""
  const cut = first.length > 80 ? `${first.slice(0, 79)}…` : first
  const summary = cut === title.trim() ? "" : cut
  return { summary, folded: summary !== text && text !== title.trim() }
}
export const fixOnDeviceLabel = (device: string) => `去设置「${device}」`

/** Where a task card sits on the tasks page, in both apps. */
export type TaskGroup = "attention" | "working" | "waiting"
export const taskGroups: readonly { group: TaskGroup; title: string }[] = [{ group: "attention", title: "需要留意" }, { group: "working", title: "进行中" }, { group: "waiting", title: "等待中" }]
/** The state a task card shows: the overview names a failed work only as finished, so its outcome says how. */
export const cardState = (work: WorkRecord, nodes: NodeInfo[], projected?: string) =>
  confirmReason(work, nodes) ? "awaiting_confirmation" : work.outcome ?? work.stop ?? (work.state === "finished" ? "unknown" : projected ?? work.state)
export const needsLook = (state: string) => ["failed", "rejected", "dispatch_rejected", "dispatch_unverified"].includes(state)
/** A task that failed, was never taken or awaits the person's confirmation needs them; one out of their hands or held on them waits; the rest work. */
export const taskGroup = (state: string, heldOnPerson: boolean): TaskGroup =>
  needsLook(state) || ["awaiting_confirmation", "unverified", "unknown"].includes(state) ? "attention" : heldOnPerson || !taskActive(state) ? "waiting" : "working"

export type SummaryStep = { state: "done" | "current" | "pending"; text: string }

/** Only a summary made entirely of marked lines is a step list. It is the latest summary, not execution history. */
export function parseSummarySteps(summary: string): SummaryStep[] | null {
  if (!summary) return null
  const steps: SummaryStep[] = []
  for (const line of summary.split(/\r?\n/)) {
    const match = /^([✓→·]) +(.+)$/.exec(line)
    if (!match || !match[2].trim()) return null
    steps.push({ state: match[1] === "✓" ? "done" : match[1] === "→" ? "current" : "pending", text: match[2] })
  }
  return steps
}

const schedulerStates: Record<string, { label: string; group: "open" | "history" }> = {
  queued: { label: "排队中", group: "open" }, claimed: { label: "已领取", group: "open" }, running: { label: "执行中", group: "open" },
  steering: { label: "补充中", group: "open" }, cancelling: { label: "停止中", group: "open" }, publishing: { label: "回复同步中", group: "open" },
  pending_delivery: { label: "等待设备接收", group: "open" }, steered: { label: "已补充", group: "history" }, completed: { label: "已完成", group: "history" },
  failed: { label: "失败", group: "history" }, cancelled: { label: "已取消", group: "history" }, interrupted: { label: "已中断", group: "history" },
}
export const schedulerStatus = (state: string): { label: string; group: "open" | "history" | "unknown" } => schedulerStates[state] ?? { label: "状态未知", group: "unknown" }

/** Muting is independent of the device's reachability and never proves that a provider session is alive. */
export function externalSessionStatus(session: Pick<SessionHandoff, "node">, nodes: readonly NodeInfo[]): "revoked" | "unknown" | "offline" | "reachable" {
  const node = nodes.find((item) => item.device_id === session.node)
  return node?.revoked ? "revoked" : !node ? "unknown" : !node.online ? "offline" : "reachable"
}
export const externalSessionLabels = { revoked: "设备已撤权", unknown: "设备可达性未知", offline: "设备不可达", reachable: "设备可达" }

export function overviewScope(coverage: OverviewCoverage): string[] {
  const notes = [coverage.source === "workspace_overview" ? "当前已同步到接入设备的记录" : "当前已加载的本地记录"]
  if (!coverage.complete || coverage.consistency === "partial") notes.push("部分记录，不代表全部任务")
  if (coverage.truncated) notes.push("总览已截断，保留其他已知任务")
  const omitted = [["任务", coverage.omitted_tasks], ["问题", coverage.omitted_questions], ["检查", coverage.omitted_checks], ["派发", coverage.omitted_dispatches], ["投递", coverage.omitted_deliveries], ["调度记录", coverage.omitted_jobs]] as const
  for (const [label, count] of omitted) if (count !== null && count > 0) notes.push(`另有 ${count} 项${label}未列出`)
  if (coverage.remote_owners.some((owner) => owner.reason === "remote_scheduler_not_queried" || owner.status === "remote_scheduler_not_queried")) notes.push("远端调度尚未查询")
  if (coverage.unreachable_nodes.length) notes.push("部分设备不可达，状态可能滞后")
  return notes
}
