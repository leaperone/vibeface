export const roleNames: Record<string, string> = {
  gateway: "接入",
  relay: "中继",
  replica: "同步",
  scheduler: "调度",
  executor: "执行",
}

export const stateNames: Record<string, string> = {
  accepted: "已接收",
  running: "执行中",
  unknown: "结果未知",
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

export function nodeName(id: string | undefined) {
  if (!id) return "未知设备"
  return `${id.slice(0, 10)}${id.length > 10 ? "…" : ""}`
}

export function stateLabel(value: string | undefined, override?: string) {
  if (override) return override
  if (!value) return "未知"
  return stateNames[value] || value
}

export function textOf(value: unknown) {
  return typeof value === "string" ? value : JSON.stringify(value, null, 2)
}

export function missingCount(value: unknown) {
  if (Array.isArray(value)) return value.length
  return typeof value === "number" ? value : 0
}

export function isIncomplete(missing: { events?: unknown; blobs?: unknown; conflicts?: unknown } | undefined) {
  return !missing || (["events", "blobs", "conflicts"] as const).some((key) => {
    const value = missing[key]
    return !Array.isArray(value) || value.length > 0
  })
}

export type FrontierHead = string

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
