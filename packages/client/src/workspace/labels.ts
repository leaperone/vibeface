import type { MembershipView } from "../lib/node.ts"
import { confirmReason } from "./model.ts"
import type { NodeInfo, WorkRecord } from "./types.ts"

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

export const hostOf = (address: string) => { try { return new URL(address).host } catch { return "" } }

const taskStates: Record<string, string> = {
  pending_delivery: "等待中", queued: "等待中", claimed: "准备中", accepted: "准备中", running: "运行中", waiting: "需要你同意",
  cancelling: "停止中", interrupting: "停止中", completed: "已完成", failed: "失败", cancelled: "已停止", interrupted: "已中断",
  pending_check: "待确认", unknown: "待确认", starting: "准备中", finished: "已完成", unverified: "待确认", awaiting_confirmation: "待确认",
  abandoned: "已放弃、未执行", dispatch_waiting: "等设备接收", dispatch_rejected: "未接收", stop_waiting: "停止中", stop_timed_out: "还在尝试停止",
}
export const taskLabel = (state: string) => taskStates[state] ?? stateNames[state] ?? state

// Live progress reports provider words such as "running" or "awaiting_input"; anything unknown reads as plain progress.
const progressStates: Record<string, string> = {
  running: "进行中", in_progress: "进行中", finishing: "收尾中", completed: "已完成", failed: "失败", interrupted: "已中断",
  unknown: "结果未知", awaiting_input: "待处理", declined: "已拒绝",
}
export const progressLabel = (status: string) => progressStates[status] ?? (/[\u4e00-\u9fff]/.test(status) ? status : "进行中")

/**
 * A work only the person can settle now reads as awaiting confirmation; a finished one as its outcome; a stop not yet
 * confirmed as that stop, which keeps being retried.
 */
export const shownState = (work: WorkRecord, nodes: NodeInfo[] = []) =>
  confirmReason(work, nodes) ? "awaiting_confirmation" : work.outcome ?? work.stop ?? work.state
export const taskActive = (state: string) => ["accepted", "running", "interrupting", "dispatch_waiting", "stop_waiting", "stop_timed_out"].includes(state)
