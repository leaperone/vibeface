import { CircleQuestionMark, Hand, MonitorSmartphone, Sparkles, MessageSquareWarning, type LucideIcon } from "lucide-react"
import { useIgnoredPairs } from "@/features/devices/ignored"
import { useDevice } from "@/features/identity/context"
import type { PairReceipt } from "@/lib/node"
import { useWorkspace } from "./context"
import type { HumanRequest, NativeSessionRecord } from "./types"

export type InboxItem = { key: string; icon: LucideIcon; title: string; meta?: string; to: string; kind: "approval" | "confirm" | "join" | "setup" | "attention"; approval?: { session: NativeSessionRecord; request: HumanRequest }; pair?: PairReceipt }

/**
 * Everything waiting on the person, newest kinds first.
 * ponytail: approvals and checks come from the open conversation only; other assistants surface through their notifications.
 */
export function useInbox(): InboxItem[] {
  const workspace = useWorkspace()
  const device = useDevice()
  const bot = workspace.selectedBot
  const data = workspace.data
  const ignored = useIgnoredPairs()
  const items: InboxItem[] = []
  const chat = bot ? `/b/${bot.bot_id}` : "/"
  for (const session of data?.native_sessions ?? []) {
    for (const request of session.pending_requests) items.push({ key: `approval:${session.work_id}:${request.request_id}`, icon: Hand, title: "需要你同意", meta: bot?.name, to: chat, kind: "approval", approval: { session, request } })
  }
  const checks = (data?.pending_checks.length ?? 0) + (data?.agent_tasks.filter((task) => task.state === "pending_check").length ?? 0)
  if (checks) items.push({ key: "confirm", icon: CircleQuestionMark, title: checks > 1 ? `${checks} 件事需要确认` : "需要确认", meta: bot?.name, to: chat, kind: "confirm" })
  if (workspace.messagePending && !workspace.messageBusy) items.push({ key: "undelivered", icon: CircleQuestionMark, title: "消息可能没送达", meta: bot?.name, to: chat, kind: "confirm" })
  if (device.ready?.principal.can_manage) {
    for (const pair of device.pairs.filter((item) => !ignored.includes(item.request_id))) items.push({ key: `join:${pair.request_id}`, icon: MonitorSmartphone, title: `${pair.review.new_member.name || "新设备"} 想加入`, to: "/devices", kind: "join", pair })
  }
  for (const note of workspace.notifications) {
    if (note.kind !== "manager_attention") continue
    const owner = workspace.bots.find((item) => item.conversation_id === note.conversation_id)
    if (owner) items.push({ key: `attention:${note.notification_id}`, icon: MessageSquareWarning, title: "助手需要你看看", meta: owner.name, to: `/b/${owner.bot_id}`, kind: "attention" })
  }
  if (workspace.nodes.length && !workspace.nodes.some((node) => !node.revoked && node.capabilities?.manager)) items.push({ key: "setup", icon: Sparkles, title: "还没配置助手", to: device.ready ? `/devices/${device.ready.entry.peer_id}` : "/devices", kind: "setup" })
  return items
}
