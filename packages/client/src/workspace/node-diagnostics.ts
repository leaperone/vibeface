import type { ReadyConnection } from "../lib/node.ts"
import type { NodeInfo, NodeReportSnapshot, NodeSessionPath, SelectedNodePath } from "./types.ts"

export const nodeStatusLabels = {
  online: "在线",
  unreachable: "未响应",
  unknown: "状态未知",
  checking: "检查中",
  disconnected: "连接已断开",
  stale: "状态已过期",
}

type NodeStatus = keyof typeof nodeStatusLabels

export type ConnectionHop = {
  kind: "client" | "gateway" | "relay" | "node"
  name: string
  via?: string
  address?: string
  deviceId?: string
}

type ConnectionTrace = {
  pathLabel: string | null
  hops: ConnectionHop[]
  routeNotice: string | null
}

export type NodeDiagnostics = ConnectionTrace & {
  deviceId: string
  status: NodeStatus
  checkedAt: number | null
  lastSyncedAt: number | null
  version: string | null
  system: string | null
  relay: boolean | null
  capabilitiesStale: boolean
}

type NodeTarget = { deviceId: string; name: string; entry: boolean }
type ConnectionOwner = Pick<ReadyConnection, "entry" | "info">

function transportHops(destination: ConnectionHop, path: SelectedNodePath | null, missing: string): ConnectionHop[] {
  if (path?.kind === "relay") return [
    { kind: "relay", name: "转发服务", address: path.url },
    { ...destination, via: "经转发服务" },
  ]
  return [{ ...destination, via: path ? "直连" : missing }]
}

function entryTrace(ready: ConnectionOwner, session: NodeSessionPath | null, clientLabel: string, nameFor: (id: string) => string | undefined): ConnectionTrace {
  const entry = ready.entry
  const client: ConnectionHop = { kind: "client", name: clientLabel }
  const node: ConnectionHop = { kind: "node", name: nameFor(entry.device_id) || ready.info.name, deviceId: entry.device_id }
  if (entry.kind === "lan") return {
    pathLabel: "局域网直连", hops: [client, { ...node, via: "局域网直连" }], routeNotice: null,
  }
  if (entry.kind === "local" || session?.kind === "local") return {
    pathLabel: "本机连接", hops: [client, { ...node, via: "本机连接" }], routeNotice: null,
  }
  if (session?.kind === "websocket") return {
    pathLabel: "网页直达节点", hops: [client, { ...node, via: "网页连接", address: entry.address }], routeNotice: null,
  }
  if (session?.kind === "quic" && session.upstreamDeviceId !== entry.device_id) {
    const gateway: ConnectionHop = {
      kind: "gateway", name: nameFor(session.upstreamDeviceId) || "转发节点", via: "网页连接",
      address: entry.address, deviceId: session.upstreamDeviceId,
    }
    return {
      pathLabel: session.path?.kind === "relay" ? "经转发节点和服务" : session.path ? "经转发节点 · 后段直连" : "经转发节点 · 后段未知",
      hops: [client, gateway, ...transportHops(node, session.path, "路径未知")],
      routeNotice: session.path ? null : "已识别转发节点，到当前节点这段路径尚未观测到。",
    }
  }
  return {
    pathLabel: "网页连接 · 路径未知",
    hops: [{ ...client, address: entry.address }, { ...node, via: "路径未知" }],
    routeNotice: "已通过网页地址连接，转发路径尚未确认。",
  }
}

function nodeStatus(entry: boolean, connected: boolean, node: NodeInfo | undefined, fresh: boolean, checking: boolean): NodeStatus {
  if (entry) return connected ? "online" : "disconnected"
  if (!connected) return "unknown"
  if (!node) return checking ? "checking" : "unknown"
  if (!fresh) return "stale"
  return node.online ? "online" : "unreachable"
}

function peerTrace(entry: ConnectionTrace, target: NodeTarget, node: NodeInfo | undefined, entryName: string): ConnectionTrace {
  const path = node?.online ? node.observation?.path ?? null : null
  const destination: ConnectionHop = { kind: "node", name: target.name, deviceId: target.deviceId }
  return {
    pathLabel: path?.kind === "direct" ? "节点间直连" : path ? "节点间经转发服务" : node?.online ? "路径未知" : null,
    hops: [...entry.hops, ...transportHops(destination, path, node?.online === false ? "未收到响应" : "路径未知")],
    routeNotice: [
      entry.routeNotice,
      node?.online === false ? `「${entryName}」暂时联系不到「${target.name}」。`
        : `最后一段由「${entryName}」探测${path ? "。" : "，路径尚未确认。"}`,
    ].filter(Boolean).join(" "),
  }
}

const systemNames = new Map([["macos", "macOS"], ["windows", "Windows"], ["linux", "Linux"], ["ios", "iOS"], ["android", "Android"]])

export function describeNode({ target, ready, connected, report, fresh, checking, clientLabel, nameFor }: {
  target: NodeTarget
  ready: ConnectionOwner | null
  connected: boolean
  report: NodeReportSnapshot | null
  fresh: boolean
  checking: boolean
  clientLabel: string
  nameFor: (id: string) => string | undefined
}): NodeDiagnostics {
  const node = report?.nodes.find((item) => !item.revoked && item.device_id === target.deviceId)
  const observation = node?.observation
  const status = nodeStatus(target.entry, connected, node, fresh, checking)
  const capabilities = node?.capabilities
  const session = report?.sessionPath ?? null
  let trace: ConnectionTrace = {
    pathLabel: null, hops: [],
    routeNotice: target.entry ? "重新连接后即可查看路径。" : "当前连接已断开，恢复后即可检查到这个节点的路径。",
  }
  if (ready && connected) {
    const entry = entryTrace(ready, session, clientLabel, nameFor)
    trace = target.entry ? entry : peerTrace(entry, target, node, nameFor(ready.entry.device_id) || ready.info.name)
    if (report && !fresh && trace.hops.length) {
      trace = {
        ...trace,
        pathLabel: trace.pathLabel ? `上次 · ${trace.pathLabel}` : null,
        routeNotice: ["显示上次检查的路径。", trace.routeNotice].filter(Boolean).join(" "),
      }
    }
  }
  return {
    ...trace,
    deviceId: target.deviceId, status,
    checkedAt: (target.entry ? session?.checkedAt : undefined) ?? observation?.checkedAt ?? null,
    lastSyncedAt: observation?.lastSyncedAt ?? null,
    version: capabilities?.version ?? (target.entry ? ready?.info.version : undefined) ?? null,
    system: [systemNames.get(capabilities?.os ?? "") ?? capabilities?.os, capabilities?.arch].filter(Boolean).join(" · ") || null,
    relay: capabilities?.relay ?? null,
    capabilitiesStale: capabilities !== undefined && (!fresh || node?.online !== true),
  }
}

const timeFormat = new Intl.DateTimeFormat("zh-CN", { dateStyle: "short", timeStyle: "medium", hourCycle: "h23" })
export const nodeDiagnosticTime = (timestamp: number) => timeFormat.format(timestamp)

export function nodeDiagnosticText(name: string, diagnostics: NodeDiagnostics): string {
  const { status, deviceId, pathLabel, hops, routeNotice, checkedAt, lastSyncedAt, version, system, relay, capabilitiesStale } = diagnostics
  return [
    name,
    `节点编号 ${deviceId}`,
    `状态 ${nodeStatusLabels[status]}`,
    pathLabel,
    ...hops.map((hop) => [
      hop.via, hop.name, hop.address, hop.kind === "gateway" ? hop.deviceId : null,
    ].filter(Boolean).join(" · ")),
    routeNotice,
    checkedAt !== null ? `检查时间 ${nodeDiagnosticTime(checkedAt)}` : null,
    lastSyncedAt !== null ? `最近成功同步 ${nodeDiagnosticTime(lastSyncedAt)}` : null,
    version ? `Core 版本 ${version}` : null,
    system ? `系统 ${system}` : null,
    relay === null ? null : `转发服务 ${relay ? "已启用" : "未启用"}${capabilitiesStale ? "（上次读取）" : ""}`,
  ].filter(Boolean).join("\n")
}
