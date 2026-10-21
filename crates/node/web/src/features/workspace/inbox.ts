import { BellRing, CircleQuestionMark, Copy, Lock, MonitorSmartphone, Sparkles, MessageSquareWarning, type LucideIcon } from "lucide-react"
import { useOpenJoins } from "@/features/devices/join"
import { useDevice } from "@/features/identity/context"
import type { JoinRequest } from "@/lib/node"
import { useWorkspace } from "./context"
import { pendingChecks } from "./model"
import type { Alert } from "./types"
import type { WriteCardKind } from "./write-card"

export type InboxItem = { key: string; icon: LucideIcon; title: string; meta?: string; to: string; kind: "alert" | "confirm" | "join" | "setup" | "attention" | "write"; alert?: Alert; join?: JoinRequest; write?: WriteCardKind }

/**
 * Everything waiting on the person, newest kinds first.
 * ponytail: checks come from the open conversation only; other assistants surface through their notifications.
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
  for (const alert of workspace.alerts) if (alert.resolution === null) items.push({ key: `alert:${alert.alert_id}`, icon: BellRing, title: alert.summary, to: "/inbox", kind: "alert", alert })
  const checks = pendingChecks(data).length
  if (checks) items.push({ key: "confirm", icon: CircleQuestionMark, title: checks > 1 ? `${checks} 件事需要确认` : "需要确认", meta: bot?.name, to: chat, kind: "confirm" })
  if (workspace.messagePending && !workspace.messageBusy) items.push({ key: "undelivered", icon: CircleQuestionMark, title: "消息可能没送达", meta: bot?.name, to: chat, kind: "confirm" })
  for (const join of joins) items.push({ key: `join:${join.join_id}`, icon: MonitorSmartphone, title: `${join.name || "新设备"} 想加入`, to: "/devices", kind: "join", join })
  for (const note of workspace.notifications) {
    if (note.kind !== "manager_attention") continue
    const owner = workspace.bots.find((item) => item.conversation_id === note.conversation_id)
    if (owner) items.push({ key: `attention:${note.notification_id}`, icon: MessageSquareWarning, title: "助手需要你看看", meta: owner.name, to: `/b/${owner.bot_id}`, kind: "attention" })
  }
  if (workspace.nodes.length && !workspace.nodes.some((node) => !node.revoked && node.capabilities?.manager)) items.push({ key: "setup", icon: Sparkles, title: "还没设置 AI 服务", to: device.ready ? `/devices/${device.ready.entry.device_id}` : "/devices", kind: "setup" })
  return items
}
