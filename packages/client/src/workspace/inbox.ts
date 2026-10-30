import { recoveryCopy } from "../identity/copy.ts"
import type { ClientPrincipal, JoinRequest } from "../lib/node.ts"
import { failedWork } from "./labels.ts"
import { botReadiness, workChecks } from "./model.ts"
import { openQuestions } from "./questions.ts"
import type { BotRecord, CoordinationCheck, NodeInfo, NotificationRecord, OpenWork, PendingCheck, Question, WriteStatus } from "./types.ts"

export type WriteCardKind = "read_only" | "forked" | "conflicts"

/** One thing waiting on the person. `done`: a question this device just answered, shown until the network's list drops it. */
export type InboxItem = { key: string; title: string; meta?: string } & (
  | { kind: "recovery" } | { kind: "rotate" } | { kind: "undelivered" }
  | { kind: "write"; write: WriteCardKind }
  | { kind: "question"; question: Question; done: boolean }
  | { kind: "confirm"; check: PendingCheck }
  | { kind: "join"; join: JoinRequest }
  | { kind: "attention"; bot: BotRecord }
  | { kind: "failed"; work: OpenWork; bot: BotRecord }
  | { kind: "setup"; device: string | undefined }
)

/** The device and workspace state 待处理 reads, as both the web and the phone hold it. */
export type InboxSource = {
  device: {
    ready: { principal: ClientPrincipal; entry: { device_id: string } } | null
    joins: readonly JoinRequest[]; pendingRecovery: unknown; rotateHint: boolean
  }
  workspace: {
    writeState: WriteStatus; questions: readonly Question[]; answered: Readonly<Record<string, string>>
    openWorks: OpenWork[]; nodes: NodeInfo[]; data: { checks: CoordinationCheck[] } | null
    messagePending: unknown; messageBusy: boolean; selectedBot: BotRecord | null
    notifications: readonly NotificationRecord[]; bots: readonly BotRecord[]
  }
}

/** Join requests a manager can still act on. */
export const openJoins = (device: InboxSource["device"]) =>
  device.ready?.principal.can_manage ? device.joins.filter((request) => request.status === "pending" || request.status === "confirming") : []

/**
 * Everything waiting on the person: write state, then every unanswered question urgent and newest first, then the rest.
 * Each work waiting for confirmation shows once, the works one unreachable device holds on one item; leftover processes
 * come from the open conversation only.
 */
export function inboxItems({ device, workspace }: InboxSource): InboxItem[] {
  const items: InboxItem[] = []
  const write = workspace.writeState
  if (device.pendingRecovery) items.push({ key: "recovery", kind: "recovery", title: recoveryCopy.pending })
  if (device.rotateHint) items.push({ key: "rotate", kind: "rotate", title: "换个恢复密码" })
  if (write.state === "read_only") items.push({ key: "write:read_only", kind: "write", write: "read_only", title: "这台电脑暂时只读" })
  if (write.state === "forked") items.push({ key: "write:forked", kind: "write", write: "forked", title: "这台电脑的数据被复制过" })
  else if (write.conflicts.length) items.push({ key: "write:conflicts", kind: "write", write: "conflicts", title: "有设备的数据被复制过" })
  for (const question of openQuestions(workspace.questions)) items.push({
    key: `question:${question.question_id}`, kind: "question", question, title: question.text, meta: question.bot?.name, done: question.question_id in workspace.answered,
  })
  const checks = [...workChecks(workspace.openWorks, workspace.nodes), ...(workspace.data?.checks ?? []).map((check): PendingCheck => ({ kind: "process", check_id: check.check_id, detail: check.detail }))]
  for (const check of checks) items.push({ key: check.check_id, kind: "confirm", check, title: "待确认" })
  // The Node lists a failure until a later task ends, the bot replies after it or the person closes it.
  for (const work of workspace.openWorks) {
    const bot = workspace.bots.find((item) => item.conversation_id === work.conversation_id)
    if (bot && failedWork(work)) items.push({ key: `failed:${work.work_id}`, kind: "failed", work, bot, title: work.title || "任务没有完成", meta: bot.name })
  }
  if (workspace.messagePending && !workspace.messageBusy) items.push({ key: "undelivered", kind: "undelivered", title: "消息可能没送达", meta: workspace.selectedBot?.name })
  for (const join of openJoins(device)) items.push({ key: `join:${join.join_id}`, kind: "join", join, title: `${join.name || "新设备"} 想加入` })
  for (const note of workspace.notifications) {
    if (note.kind !== "manager_attention") continue
    const bot = workspace.bots.find((item) => item.conversation_id === note.conversation_id)
    if (bot) items.push({ key: `attention:${note.notification_id}`, kind: "attention", bot, title: note.text.trim() || "Bot 需要你看看", meta: bot.name })
  }
  const unready = workspace.nodes.find((node) => botReadiness(node) === "needs_setup")
  // Only an admin, or this computer's own page, may set one up; elsewhere the item would lead nowhere.
  const canSetUp = device.ready?.principal.can_manage === true || device.ready?.principal.source === "local_session"
  // A device still starting may come up on its own, but it does not hide another one that needs setting up.
  const waiting = !unready && workspace.nodes.some((node) => botReadiness(node) === "starting")
  if (canSetUp && workspace.nodes.length && !waiting && !workspace.nodes.some((node) => botReadiness(node) === "ready")) {
    items.push({ key: "setup", kind: "setup", device: unready?.device_id ?? device.ready?.entry.device_id, title: unready ? "运行 Bot 的设备需设置" : "设置运行 Bot 的设备" })
  }
  return items
}

/** What 待处理 counts: every item but a question this device already answered. */
export const waitingItems = (items: readonly InboxItem[]) => items.filter((item) => !(item.kind === "question" && item.done))
