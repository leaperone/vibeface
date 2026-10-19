import { CircleQuestionMark, Hand, MonitorSmartphone, Sparkles, MessageSquareWarning, type LucideIcon } from "lucide-react"
import { useOpenJoins } from "@/features/devices/join"
import { useDevice } from "@/features/identity/context"
import type { JoinRequest } from "@/lib/node"
import { useWorkspace } from "./context"
import type { WorkApproval, WorkRecord } from "./types"

export type InboxItem = { key: string; icon: LucideIcon; title: string; meta?: string; to: string; kind: "approval" | "confirm" | "join" | "setup" | "attention"; approval?: { work: WorkRecord; request: WorkApproval }; join?: JoinRequest }

/**
 * Everything waiting on the person, newest kinds first.
 * ponytail: approvals and checks come from the open conversation only; other assistants surface through their notifications.
 */
export function useInbox(): InboxItem[] {
  const workspace = useWorkspace()
  const device = useDevice()
  const bot = workspace.selectedBot
  const data = workspace.data
  const joins = useOpenJoins()
  const items: InboxItem[] = []
  const chat = bot ? `/b/${bot.bot_id}` : "/"
  for (const work of data?.works ?? []) {
    for (const request of work.open_approvals) items.push({ key: `approval:${work.work_id}:${request.request_id}`, icon: Hand, title: "需要你同意", meta: bot?.name, to: chat, kind: "approval", approval: { work, request } })
  }
  const checks = data?.pending_checks.length ?? 0
  if (checks) items.push({ key: "confirm", icon: CircleQuestionMark, title: checks > 1 ? `${checks} 件事需要确认` : "需要确认", meta: bot?.name, to: chat, kind: "confirm" })
  if (workspace.messagePending && !workspace.messageBusy) items.push({ key: "undelivered", icon: CircleQuestionMark, title: "消息可能没送达", meta: bot?.name, to: chat, kind: "confirm" })
  for (const join of joins) items.push({ key: `join:${join.join_id}`, icon: MonitorSmartphone, title: `${join.name || "新设备"} 想加入`, to: "/devices", kind: "join", join })
  for (const note of workspace.notifications) {
    if (note.kind !== "manager_attention") continue
    const owner = workspace.bots.find((item) => item.conversation_id === note.conversation_id)
    if (owner) items.push({ key: `attention:${note.notification_id}`, icon: MessageSquareWarning, title: "助手需要你看看", meta: owner.name, to: `/b/${owner.bot_id}`, kind: "attention" })
  }
  if (workspace.nodes.length && !workspace.nodes.some((node) => !node.revoked && node.capabilities?.manager)) items.push({ key: "setup", icon: Sparkles, title: "还没配置助手", to: device.ready ? `/devices/${device.ready.entry.device_id}` : "/devices", kind: "setup" })
  return items
}
