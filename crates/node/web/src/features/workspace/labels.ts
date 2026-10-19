import type { MembershipView } from "@/lib/node"
import type { NodeInfo, WorkRecord } from "./types"

export const roleNames: Record<string, string> = {
  gateway: "对外访问",
  relay: "转发",
  replica: "保存副本",
  scheduler: "安排任务",
}

export const stateNames: Record<string, string> = {
  accepted: "已接收",
  pending_delivery: "等待设备接收",
  queued: "等待处理",
  claimed: "正在准备",
  running: "执行中",
  waiting: "等你决定",
  cancelling: "正在确认是否已取消",
  interrupting: "正在确认是否已停止",
  interrupted: "已中断",
  starting: "正在启动",
  unverified: "需要确认",
  stopped: "已停止",
  unknown: "结果未知",
  pending_check: "需要确认",
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
  awaiting_input: "等你处理",
  finishing: "收尾中",
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

export function isLoopback(address: string) {
  try { return ["localhost", "127.0.0.1", "[::1]"].includes(new URL(address).hostname) } catch { return false }
}

/** Short display name for an access address. */
export function addressName(address: string) {
  if (isLoopback(address)) return "这台电脑"
  try { return new URL(address).hostname } catch { return address }
}

const taskStates: Record<string, string> = {
  pending_delivery: "等待中", queued: "等待中", claimed: "准备中", accepted: "准备中", running: "运行中", waiting: "需要你同意",
  cancelling: "停止中", interrupting: "停止中", completed: "已完成", failed: "失败", cancelled: "已停止", interrupted: "已中断",
  pending_check: "需要确认", unknown: "需要确认", starting: "准备中", waiting_approval: "需要你同意", finished: "已完成", unverified: "需要确认",
}
export const taskLabel = (state: string) => taskStates[state] ?? stateNames[state] ?? state

// Live progress reports provider words such as "running" or "awaiting_input"; anything unknown reads as plain progress.
const progressStates: Record<string, string> = {
  running: "进行中", in_progress: "进行中", finishing: "收尾中", completed: "已完成", failed: "失败", interrupted: "已中断",
  unknown: "结果未知", awaiting_input: "等你处理", declined: "已拒绝",
}
export const progressLabel = (status: string) => progressStates[status] ?? (/[\u4e00-\u9fff]/.test(status) ? status : "进行中")

const decisions: Record<string, string> = { accept: "已同意", decline: "已拒绝", cancel: "已拒绝并停止", answered: "已回答" }
export const decisionLabel = (decision: string) => decisions[decision] ?? "已处理"
/** A finished work reads as its outcome; one left waiting with nothing to answer reads as running. */
export const shownState = (work: Pick<WorkRecord, "state" | "outcome" | "open_approvals">) =>
  work.outcome ?? (work.state === "waiting_approval" && !work.open_approvals.length ? "running" : work.state ?? "accepted")
export const taskActive = (state: string) => ["accepted", "starting", "running", "waiting_approval", "interrupting"].includes(state)

const kinds: Record<string, string> = {
  "item/commandExecution/requestApproval": "运行命令",
  "item/fileChange/requestApproval": "修改文件",
  "item/tool/requestUserInput": "补充信息",
}
export const approvalKind = (request: { kind: string }) => kinds[request.kind] ?? "处理请求"

// ponytail: keyword heuristics; a command parser if these miss real cases.
const risks: [RegExp, string][] = [
  [/git\s+push\b.*(--force|\s-f\b)/, "会覆盖远端的提交记录"],
  [/(^|[\s;&|])(rm|rmdir)\s|git\s+clean\b|git\s+reset\s+--hard/, "会删除文件"],
  [/(^|[\s;&|])(sudo|chmod|chown)\s/, "会改动权限"],
  [/(^|[\s;&|])(curl|wget)\s|\b(npm|pnpm|yarn|bun)\s+(i|install|add)\b|\bpip3?\s+install\b|\b(brew|cargo)\s+install\b/, "会从网络下载"],
]

/** One line describing the consequence of a risky request, or "" when nothing stands out. */
export function approvalRisk(request: { command?: string; cwd?: string }, roots: string[]): string {
  const hit = risks.find(([pattern]) => pattern.test(request.command ?? ""))
  if (hit) return hit[1]
  const cwd = request.cwd
  if (cwd && roots.length && !roots.some((root) => cwd === root || cwd.startsWith(root.endsWith("/") ? root : `${root}/`))) return "在可用文件夹之外"
  return ""
}
