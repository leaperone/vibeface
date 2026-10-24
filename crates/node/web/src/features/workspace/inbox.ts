import { CircleQuestionMark, Copy, Lock, MessageCircleQuestionMark, MonitorSmartphone, Sparkles, MessageSquareWarning, type LucideIcon } from "lucide-react"
import { useOpenJoins } from "@/features/devices/join"
import { useDevice } from "@vibeface/client/identity/context.tsx"
import type { JoinRequest } from "@vibeface/client/lib/node.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { botReadiness, workChecks } from "@vibeface/client/workspace/model.ts"
import { openQuestions } from "@vibeface/client/workspace/questions.ts"
import type { PendingCheck, Question } from "@vibeface/client/workspace/types.ts"
import type { WriteCardKind } from "./write-card"

/** `done`: a question this browser just answered, shown collapsed until the network's list drops it. */
export type InboxItem = { key: string; icon: LucideIcon; title: string; meta?: string; to: string; kind: "question" | "confirm" | "join" | "setup" | "attention" | "write"; question?: Question; check?: PendingCheck; done?: boolean; join?: JoinRequest; write?: WriteCardKind }

/**
 * Everything waiting on the person: write state, then every unanswered question newest first, then the rest.
 * Each work waiting for confirmation shows once, the works one unreachable device holds on one card; leftover processes
 * come from the open conversation only, each on its own card.
 */
export function useInbox(): InboxItem[] {
  const workspace = useWorkspace()
  const device = useDevice()
  const bot = workspace.selectedBot
  const data = workspace.data
  const joins = useOpenJoins()
  const items: InboxItem[] = []
  const chat = bot ? `/b/${bot.bot_id}` : "/"
  const write = workspace.writeState
  if (write.state === "read_only") items.push({ key: "write:read_only", icon: Lock, title: "这台设备暂时只读", to: "/inbox", kind: "write", write: "read_only" })
  if (write.state === "forked") items.push({ key: "write:forked", icon: Copy, title: "这台设备的数据被复制过", to: "/inbox", kind: "write", write: "forked" })
  else if (write.conflicts.length) items.push({ key: "write:conflicts", icon: Copy, title: "有设备的数据被复制过", to: "/inbox", kind: "write", write: "conflicts" })
  for (const question of openQuestions(workspace.questions)) items.push({
    key: `question:${question.question_id}`, icon: MessageCircleQuestionMark, title: question.text, meta: question.bot?.name,
    to: question.bot ? `/b/${question.bot.bot_id}` : "/inbox", kind: "question", question, done: question.question_id in workspace.answered,
  })
  const checks = [...workChecks(workspace.openWorks, workspace.nodes), ...(data?.checks ?? []).map((check): PendingCheck => ({ kind: "process", check_id: check.check_id, detail: check.detail }))]
  for (const check of checks) items.push({ key: check.check_id, icon: CircleQuestionMark, title: "待确认", to: "/inbox", kind: "confirm", check })
  if (workspace.messagePending && !workspace.messageBusy) items.push({ key: "undelivered", icon: CircleQuestionMark, title: "消息可能没送达", meta: bot?.name, to: chat, kind: "confirm" })
  for (const join of joins) items.push({ key: `join:${join.join_id}`, icon: MonitorSmartphone, title: `${join.name || "新设备"} 想加入`, to: "/devices", kind: "join", join })
  for (const note of workspace.notifications) {
    if (note.kind !== "manager_attention") continue
    const owner = workspace.bots.find((item) => item.conversation_id === note.conversation_id)
    if (owner) items.push({ key: `attention:${note.notification_id}`, icon: MessageSquareWarning, title: note.text.trim() || "Bot 需要你看看", meta: owner.name, to: `/b/${owner.bot_id}`, kind: "attention" })
  }
  const unready = workspace.nodes.find((node) => botReadiness(node) === "needs_setup")
  const setupDevice = unready?.device_id ?? device.ready?.entry.device_id
  if (workspace.nodes.length && !workspace.nodes.some((node) => botReadiness(node) === "ready")) items.push({ key: "setup", icon: Sparkles, title: unready ? "Bot 需设置" : "还没设置 Bot", to: setupDevice ? `/devices/${setupDevice}` : "/devices", kind: "setup" })
  return items
}
