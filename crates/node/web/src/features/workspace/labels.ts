import type { MembershipView } from "@/lib/node"
import type { NodeInfo } from "./types"

export const roleNames: Record<string, string> = {
  gateway: "接入",
  relay: "中继",
  replica: "同步",
  scheduler: "调度",
}

export const stateNames: Record<string, string> = {
  accepted: "已接收",
  pending_delivery: "等待设备接收",
  queued: "等待处理",
  claimed: "正在准备",
  running: "执行中",
  waiting: "等待本人决定",
  cancelling: "正在核对取消结果",
  interrupting: "正在核对停止结果",
  interrupted: "已中断",
  unknown: "结果未知",
  pending_check: "待核对",
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
}

export type NodeNameLookup = {
  nodes?: readonly NodeInfo[]
  directory?: MembershipView | null
}

export function nodeName(
  id: string | undefined,
  lookup?: NodeNameLookup,
  fallback?: string,
) {
  if (!id) return "未知设备"
  const node = lookup?.nodes?.find((item) => item.peer_id === id)
  if (node?.name) return node.name
  const member = lookup?.directory?.members.find((item) => item.subject_id === id)
  if (member?.name) return member.name
  if (node?.local) return "本机"
  if (fallback) return fallback
  return `${id.slice(0, 10)}${id.length > 10 ? "…" : ""}`
}

export function stateLabel(value: string | undefined, override?: string) {
  if (override) return override
  if (!value) return "未知"
  return stateNames[value] || value
}

export function isIncomplete(missing: { events?: unknown; blobs?: unknown; conflicts?: unknown } | undefined) {
  return !missing || (["events", "blobs", "conflicts"] as const).some((key) => {
    const value = missing[key]
    return !Array.isArray(value) || value.length > 0
  })
}

export function mergeBasis(current: readonly string[], basisText: string | null) {
  if (!basisText || basisText.length >= 6000) return current
  try {
    const heads: unknown = JSON.parse(basisText)
    if (!Array.isArray(heads) || !heads.every((head) => typeof head === "string")) return current
    return [...new Set([...current, ...heads])]
  } catch {
    return current
  }
}
