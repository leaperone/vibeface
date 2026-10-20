import type { TranscriptMessage } from "../chat/types"
import type { ReplyStatus } from "../chat/types"
import type {
  Alert, ConversationPayload, ConversationSummary, DecisionResult, NodeInfo, NotificationRecord, Outlet, Severity, TriageSettings,
  SchedulerSnapshot, ConflictVersion, WriteStatus, WorkRecord, WorkState, WorkOutcome, BotRecord, CreateBotRequest, PendingBot, PendingCheck,
} from "./types"

export function inputText(value: string, label: string, maxBytes: number): string {
  const normalized = value.trim()
  if (!normalized) throw new Error(`${label}不能为空。`)
  if (new TextEncoder().encode(normalized).length > maxBytes) throw new Error(`${label}不能超过 ${maxBytes} 字节。`)
  return normalized
}

export function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("设备返回的内容格式不对。")
  return value as Record<string, unknown>
}
function text(value: unknown): string {
  if (typeof value !== "string") throw new Error("设备返回的内容缺少文字。")
  return value
}
function number(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new Error("设备返回的数字不对。")
  return value
}
function flag(value: unknown): boolean {
  if (typeof value !== "boolean") throw new Error("设备返回的内容缺少开关值。")
  return value
}
function list(value: unknown): unknown[] {
  if (!Array.isArray(value)) throw new Error("设备返回的内容缺少列表。")
  return value
}
function texts(value: unknown): string[] { return list(value).map(text) }
function conversation(value: unknown): ConversationSummary {
  const data = record(value)
  return { conversation_id: text(data.conversation_id), title: data.title == null ? "" : text(data.title), owner: data.owner == null ? null : text(data.owner), owner_epoch: number(data.owner_epoch) }
}
export function parseNodes(value: unknown): NodeInfo[] {
  return list(record(value).nodes).map((item) => {
    const data = record(item)
    const capabilities = data.capabilities == null ? null : record(data.capabilities)
    return {
      device_id: text(data.device_id),
      name: typeof data.name === "string" ? data.name : "",
      local: flag(data.local), online: flag(data.online), revoked: flag(data.revoked), roles: texts(data.roles),
      capabilities: capabilities ? { manager: capabilities.manager === null ? null : text(capabilities.manager), roots: texts(capabilities.roots) } : undefined,
    }
  })
}
export function parseNotifications(value: unknown): NotificationRecord[] {
  return list(record(value).notifications).map((item) => {
    const data = record(item)
    return { notification_id: text(data.notification_id), conversation_id: text(data.conversation_id), kind: text(data.kind), text: text(data.text) }
  })
}
function message(value: unknown): TranscriptMessage {
  const data = record(value)
  return { message_id: text(data.message_id), role: text(data.role), text: text(data.text), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms), in_reply_to: optionalText(data.in_reply_to), status: data.status == null ? undefined : replyStatus(data.status), blobs: blobReferences(data.blobs) }
}
function replyStatus(value: unknown): ReplyStatus {
  switch (value) {
    case "completed": case "stopped": case "interrupted": case "failed": return value
    default: throw new Error("未知的回复状态。")
  }
}
function blobReferences(value: unknown) { return value == null ? undefined : list(value).map((item) => { const blob = record(item); return { hash: text(blob.hash), size: number(blob.size) } }) }
function optionalText(value: unknown): string | undefined { return value == null ? undefined : text(value) }
function schedulerSnapshot(value: unknown): SchedulerSnapshot | null {
  if (value == null) return null
  const data = record(value), capabilities = data.capabilities == null ? null : record(data.capabilities)
  return { jobs: list(data.jobs).map((item) => { const job = record(item); return { trigger: text(job.trigger), state: text(job.state), message_id: optionalText(job.message_id), reply_message_id: optionalText(job.reply_message_id), error: optionalText(job.error), pending_check: job.pending_check === true } }), provider: optionalText(data.provider), model: optionalText(data.model), capabilities: capabilities ? { streaming: flag(capabilities.streaming), cancel: flag(capabilities.cancel), steer: flag(capabilities.steer) } : undefined }
}
function workState(value: unknown): WorkState | null {
  switch (value) {
    case undefined: case null: return null
    case "accepted": case "starting": case "running": case "interrupting": case "finished": case "unverified": case "waiting_for_device": case "rejected": return value
    default: throw new Error("任务状态无法识别。")
  }
}
function workOutcome(value: unknown): WorkOutcome | null {
  switch (value) {
    case undefined: case null: return null
    case "completed": case "failed": case "interrupted": case "rejected": case "unverified": case "abandoned": return value
    default: throw new Error("任务结果无法识别。")
  }
}
function work(value: unknown): WorkRecord {
  const data = record(value)
  return {
    work_id: text(data.work_id), trigger: text(data.trigger), target_node: text(data.target_node), cwd: text(data.cwd), title: data.title == null ? "" : text(data.title), prompt: text(data.prompt),
    state: workState(data.state), summary: text(data.summary), outcome: workOutcome(data.outcome), result_text: text(data.result_text),
    failure_reason: data.failure_reason == null ? null : text(data.failure_reason),
    ...(data.stop == null ? {} : { stop: { waiting_for_device: flag(record(data.stop).waiting_for_device), timed_out: flag(record(data.stop).timed_out) } }),
  }
}
function pendingCheck(value: unknown): PendingCheck {
  const data = record(value), kind = data.kind
  if (kind !== "manager_process" && kind !== "work") throw new Error("需要确认的事项无法识别。")
  return { check_id: text(data.check_id), kind, detail: text(data.detail), process_groups: data.process_groups == null ? [] : list(data.process_groups).map(number), ...(kind === "work" ? { work_id: text(data.work_id), target_node: text(data.target_node) } : {}) }
}
export function parseConversation(value: unknown, expectedId?: string): ConversationPayload {
  const data = record(value)
  const owner = data.conversation === null ? null : conversation(data.conversation)
  if (expectedId && owner?.conversation_id !== expectedId) throw new Error("设备返回了别的对话，已暂停。")
  return {
    conversation: owner, messages: list(data.messages).map(message),
    works: list(data.works).map(work),
    scheduler: schedulerSnapshot(data.scheduler),
    pending_checks: list(data.pending_checks).map(pendingCheck),
    local_node: text(data.local_node),
  }
}
export function parseBinaryPage(value: unknown, hash: string, offset: number) {
  const data = record(value)
  const size = number(data.size)
  const bytes = Uint8Array.from(atob(text(data.data)), (char) => char.charCodeAt(0))
  const next_offset = data.next_offset === null ? null : number(data.next_offset)
  if (data.blob_hash !== hash || data.offset !== offset || bytes.length !== (next_offset ?? size) - offset) throw new Error("下载内容与原请求不符。")
  return { bytes, next_offset, size }
}

/** 1536 → "1.5 KB"; decimal units, as file managers show them. */
export function formatSize(bytes: number) {
  const units = ["字节", "KB", "MB", "GB"]
  let value = bytes, unit = 0
  while (value >= 1000 && unit < units.length - 1) { value /= 1000; unit += 1 }
  return unit === 0 ? `${bytes} 字节` : `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`
}

export function parseArtifactPage(value: unknown, hash: string, offset: number) {
  const data = record(value)
  const size = number(data.size), content = text(data.text)
  if (data.blob_hash !== hash || data.offset !== offset || offset > size) throw new Error("产物读取响应与原请求不符。")
  const next_offset = data.next_offset === null ? null : number(data.next_offset)
  if (next_offset !== null && (next_offset <= offset || next_offset > size || !content)) throw new Error("产物分页位置无效。")
  if (new TextEncoder().encode(content).length !== (next_offset ?? size) - offset) throw new Error("产物分页内容不完整。")
  return { text: content, offset, next_offset, size }
}

export function parseCreateBot(value: unknown): CreateBotRequest {
  const data = record(value), project = data.project == null ? null : record(data.project)
  return { bot_id: text(data.bot_id), conversation_id: text(data.conversation_id), name: text(data.name), instructions: text(data.instructions), project: project ? { target_node: text(project.target_node), cwd: text(project.cwd) } : undefined }
}
export function parseBot(value: unknown): BotRecord {
  return { ...parseCreateBot(value), event_hash: text(record(value).event_hash) }
}
export function parseBots(value: unknown): BotRecord[] {
  return list(record(value).bots).map(parseBot)
}

export function parsePendingBot(value: unknown): PendingBot {
  const data = record(value), submission = record(data.submission)
  if (submission.state !== "unconfirmed" && submission.state !== "rejected") throw new Error("机器人创建记录的提交状态无效。")
  return { ...parseCreateBot(value), submission: { state: submission.state, entry_node: text(submission.entry_node) } }
}

function conflictVersion(value: unknown): ConflictVersion {
  const data = record(value)
  return { hash: text(data.hash), scope_id: text(data.scope_id), created_at_ms: number(data.created_at_ms), body: record(data.body) }
}
export function parseWriteStatus(value: unknown): WriteStatus {
  const data = record(value)
  const state = data.state
  if (state !== "aligning" && state !== "read_only" && state !== "writable" && state !== "forked") throw new Error("设备返回的写入状态不对。")
  return {
    state,
    conflicts: list(data.conflicts).map((item) => {
      const conflict = record(item)
      return { author: text(conflict.author), seq: number(conflict.seq), kept: conflict.kept == null ? null : conflictVersion(conflict.kept), other: conflictVersion(conflict.other) }
    }),
  }
}

function severity(value: unknown): Severity {
  if (value !== "info" && value !== "attention" && value !== "urgent") throw new Error("提醒的紧急程度无法识别。")
  return value
}
export function parseAlerts(value: unknown): Alert[] {
  return list(record(value).alerts).map((item) => {
    const data = record(item), resolution = data.resolution
    if (resolution !== null && resolution !== "decided" && resolution !== "dismissed") throw new Error("提醒的处理结果无法识别。")
    return {
      alert_id: text(data.alert_id), matter: text(data.matter), agent: text(data.agent), session_id: text(data.session_id),
      thread_id: optionalText(data.thread_id), request_id: optionalText(data.request_id), turn_id: optionalText(data.turn_id),
      choices: data.choices == null ? [] : list(data.choices).map((choice) => ({ label: text(record(choice).label) })),
      cwd: text(data.cwd), severity: severity(data.severity), summary: text(data.summary), count: number(data.count), node: text(data.node),
      created_at_ms: number(data.created_at_ms), resolution,
    }
  })
}
export function parseDecision(value: unknown): DecisionResult {
  const data = record(value), status = data.status
  if (status !== "delivered" && status !== "unknown" && status !== "rejected") throw new Error("送达结果无法识别。")
  return { decision_id: text(data.decision_id), status, reason: optionalText(data.reason) }
}
function outlet(value: unknown): Outlet {
  if (value !== "record" && value !== "webhook" && value !== "both" && value !== "off") throw new Error("提醒去向无法识别。")
  return value
}
export function parseTriageSettings(value: unknown): TriageSettings {
  const data = record(value), outbox = record(data.outbox)
  return {
    default_outlet: outlet(data.default_outlet),
    projects: list(data.projects).map((item) => { const project = record(item); return { prefix: text(project.prefix), outlet: outlet(project.outlet) } }),
    webhook_url: data.webhook_url == null ? "" : text(data.webhook_url), scan_codex_sessions: flag(data.scan_codex_sessions),
    webhook_secret_set: flag(data.webhook_secret_set), decision_url: text(data.decision_url), device_id: text(data.device_id),
    decision_tokens: list(data.decision_tokens).map((item) => { const token = record(item); return { token_id: text(token.token_id), label: text(token.label), created_ms: number(token.created_ms) } }),
    outbox: { pending: number(outbox.pending), delivered: number(outbox.delivered), dead: number(outbox.dead) },
  }
}
