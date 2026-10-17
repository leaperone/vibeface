import type { PendingMessage, TranscriptMessage } from "../chat/types"
import type {
  ConversationPayload, ConversationSummary, NodeInfo, NotificationRecord, OperationRecord,
  OperationRequest, OperateAction, PendingOperation, SessionRecord, TaskRecord,
  PendingConversation, PendingTask, FollowupMode, HumanRequest, TaskBinding, SchedulerSnapshot, TaskControlRequest, HumanQuestion,
} from "./types"

export function inputText(value: string, label: string, maxBytes: number): string {
  const normalized = value.trim()
  if (!normalized) throw new Error(`${label}不能为空。`)
  if (new TextEncoder().encode(normalized).length > maxBytes) throw new Error(`${label}不能超过 ${maxBytes} 字节。`)
  return normalized
}

export function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("节点响应必须是对象。")
  return value as Record<string, unknown>
}
function text(value: unknown): string {
  if (typeof value !== "string") throw new Error("节点响应缺少文本字段。")
  return value
}
function number(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new Error("节点响应包含无效编号。")
  return value
}
function flag(value: unknown): boolean {
  if (typeof value !== "boolean") throw new Error("节点响应缺少布尔字段。")
  return value
}
function list(value: unknown): unknown[] {
  if (!Array.isArray(value)) throw new Error("节点响应缺少列表字段。")
  return value
}
function texts(value: unknown): string[] { return list(value).map(text) }
function conversation(value: unknown): ConversationSummary {
  const data = record(value)
  return { conversation_id: text(data.conversation_id), title: text(data.title) }
}
export function parseNodes(value: unknown): NodeInfo[] {
  return list(record(value).nodes).map((item) => {
    const data = record(item)
    const capabilities = data.capabilities == null ? null : record(data.capabilities)
    return {
      node_id: text(data.node_id), local: flag(data.local), online: flag(data.online), revoked: flag(data.revoked), roles: texts(data.roles),
      capabilities: capabilities ? { manager: capabilities.manager === null ? null : text(capabilities.manager), executors: texts(capabilities.executors), roots: texts(capabilities.roots) } : undefined,
    }
  })
}
export function parseConversations(value: unknown): ConversationSummary[] {
  return list(record(value).conversations).map(conversation)
}
export function parseNotifications(value: unknown): NotificationRecord[] {
  return list(record(value).notifications).map((item) => {
    const data = record(item)
    return { notification_id: text(data.notification_id), conversation_id: text(data.conversation_id), kind: text(data.kind), text: text(data.text) }
  })
}
function message(value: unknown): TranscriptMessage {
  const data = record(value)
  return { message_id: text(data.message_id), role: text(data.role), text: text(data.text), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms), blobs: blobReferences(data.blobs) }
}
function blobReferences(value: unknown) { return value == null ? undefined : list(value).map((item) => { const blob = record(item); return { hash: text(blob.hash), size: number(blob.size) } }) }
function optionalText(value: unknown): string | undefined { return value == null ? undefined : text(value) }
export function parseFollowupMode(value: unknown): FollowupMode {
  if (value !== "off" && value !== "report" && value !== "continue") throw new Error("节点返回了未知的后续授权。")
  return value
}
function humanRequest(value: unknown): HumanRequest {
  const data = record(value)
  const questions: HumanQuestion[] | undefined = data.questions == null ? undefined : list(data.questions).map((item) => {
    const question = record(item)
    return { id: text(question.id), header: text(question.header), question: text(question.question), is_secret: question.is_secret == null ? undefined : flag(question.is_secret), options: question.options == null ? undefined : list(question.options).map((item) => { const option = record(item); return { label: text(option.label), description: optionalText(option.description) } }) }
  })
  if (questions?.some((question) => !question.id) || questions && new Set(questions.map((question) => question.id)).size !== questions.length) throw new Error("人工问题编号无效或重复。")
  return { request_id: typeof data.request_id === "string" ? text(data.request_id) : number(data.request_id), kind: text(data.kind), title: optionalText(data.title), text: text(data.text), choices: data.choices == null ? undefined : list(data.choices).map((item) => { const choice = record(item); if (!("response" in choice)) throw new Error("人工决定缺少回复内容。"); return { label: text(choice.label), response: choice.response } }), questions }
}
function taskBinding(value: unknown): TaskBinding {
  const data = record(value)
  if (data.freshness != null && data.freshness !== "local_ledger" && data.freshness !== "cached" && data.freshness !== "unknown") throw new Error("未知的执行状态读取来源。")
  return { target_node: text(data.target_node), session_id: text(data.session_id), operation_id: optionalText(data.operation_id), state: optionalText(data.state), capabilities: texts(data.capabilities), pending_requests: list(data.pending_requests).map(humanRequest), current_turn: optionalText(data.current_turn), latest_progress: optionalText(data.latest_progress), revision: number(data.revision), freshness: data.freshness ?? undefined, checked_at_ms: data.checked_at_ms == null ? undefined : number(data.checked_at_ms) }
}
function schedulerSnapshot(value: unknown): SchedulerSnapshot | null {
  if (value == null) return null
  const data = record(value), capabilities = data.capabilities == null ? null : record(data.capabilities)
  return { jobs: list(data.jobs).map((item) => { const job = record(item); return { trigger: text(job.trigger), state: text(job.state), kind: text(job.kind), sequence: number(job.sequence), message_id: optionalText(job.message_id), target_node: optionalText(job.target_node), reply_message_id: optionalText(job.reply_message_id), read_only: job.read_only == null ? undefined : flag(job.read_only), error: optionalText(job.error), delivery_error: optionalText(job.delivery_error) } }), provider: optionalText(data.provider), model: optionalText(data.model), capabilities: capabilities ? { streaming: flag(capabilities.streaming), cancel: flag(capabilities.cancel), steer: flag(capabilities.steer) } : undefined, write_frozen: data.write_frozen == null ? undefined : flag(data.write_frozen) }
}
export function hasQueuedMessage(scheduler: SchedulerSnapshot | null, pending: PendingMessage): boolean {
  return scheduler?.jobs.some((job) => job.message_id === pending.message_id && job.target_node === pending.scheduler_node && ["queued", "claimed", "running", "cancelling", "completed", "cancelled", "failed", "unknown"].includes(job.state)) ?? false
}
function task(value: unknown): TaskRecord {
  const data = record(value)
  return { task_id: text(data.task_id), title: text(data.title), goal: text(data.goal), acceptance: text(data.acceptance), detail: text(data.detail), status: text(data.status), followup_mode: data.followup_mode == null ? undefined : parseFollowupMode(data.followup_mode), followup_version: data.followup_version == null ? undefined : number(data.followup_version), followup_scheduler_node: optionalText(data.followup_scheduler_node), binding: data.binding == null ? undefined : taskBinding(data.binding), blobs: blobReferences(data.blobs), followup_error: optionalText(data.followup_error), binding_error: optionalText(data.binding_error) }
}
export function parseSession(value: unknown): SessionRecord {
  const data = record(value)
  return { target_node: text(data.target_node), session_id: text(data.session_id), executor: text(data.executor), state: text(data.state), cwd: text(data.cwd), revision: number(data.revision), native_session_id: data.native_session_id == null ? undefined : text(data.native_session_id), capabilities: data.capabilities == null ? undefined : texts(data.capabilities) }
}
export function parseOperation(value: unknown): OperationRecord {
  const data = record(value), request = record(data.request)
  const state = text(data.state)
  if (!["accepted", "running", "unknown", "succeeded", "failed", "rejected"].includes(state)) throw new Error("未知的操作状态。")
  return { state, request: { operation_id: text(request.operation_id), target_node: text(request.target_node), conversation_id: text(request.conversation_id), session_id: text(request.session_id) }, raw: value }
}
export function parseConversation(value: unknown, expectedId?: string): ConversationPayload {
  const data = record(value), missing = record(data.missing)
  const owner = data.conversation === null ? null : conversation(data.conversation)
  if (expectedId && owner?.conversation_id !== expectedId) throw new Error("节点返回了其他会话，已暂停操作。")
  return {
    conversation: owner, messages: list(data.messages).map(message), heads: texts(data.heads),
    missing: { events: texts(missing.events), blobs: list(missing.blobs), conflicts: texts(missing.conflicts) },
    tasks: list(data.tasks).map(task), sessions: list(data.sessions).map(parseSession), operations: list(data.operations).map(parseOperation),
    scheduler: schedulerSnapshot(data.scheduler),
  }
}
export function parseAction(value: unknown): OperateAction {
  const data = record(value)
  switch (data.type) {
    case "open": return { type: "open", executor: text(data.executor), cwd: text(data.cwd) }
    case "read": return { type: "read" }
    case "send": return { type: "send", text: text(data.text) }
    case "steer": return { type: "steer", text: text(data.text), turn_id: text(data.turn_id) }
    case "interrupt": return { type: "interrupt", turn_id: text(data.turn_id) }
    case "respond_request": return { type: "respond_request", request_id: typeof data.request_id === "string" ? data.request_id : number(data.request_id), response: record(data.response) }
    default: throw new Error("未知的执行操作。")
  }
}

function operationRequest(value: unknown, networkId: string): OperationRequest {
  const data = record(value)
  if (data.network_id !== networkId) throw new Error("待核对操作属于其他个人网络。")
  return {
    operation_id: text(data.operation_id), network_id: networkId, conversation_id: text(data.conversation_id), task_id: data.task_id === null ? null : text(data.task_id),
    target_node: text(data.target_node), session_id: text(data.session_id), expected_revision: number(data.expected_revision),
    basis_heads: texts(data.basis_heads), resources: texts(data.resources), action: parseAction(data.action),
  }
}
function controlRequest(value: unknown, networkId: string): TaskControlRequest {
  const data = record(value)
  if (data.network_id !== networkId) throw new Error("待核对控制属于其他个人网络。")
  if (data.action !== "interrupt" && data.action !== "steer" && data.action !== "respond_request") throw new Error("未知的待核对任务控制。")
  const request = { operation_id: text(data.operation_id), network_id: networkId, conversation_id: text(data.conversation_id), task_id: text(data.task_id), target_node: text(data.target_node), session_id: text(data.session_id), expected_operation_id: text(data.expected_operation_id), expected_revision: number(data.expected_revision), action: data.action }
  if (data.action === "steer") return { ...request, action: "steer", text: text(data.text) }
  if (data.action === "respond_request") {
    if (!("response" in data)) throw new Error("待核对审批缺少本人决定。")
    return { ...request, action: "respond_request", request_id: typeof data.request_id === "string" ? text(data.request_id) : number(data.request_id), response: data.response }
  }
  return { ...request, action: "interrupt" }
}
export function parsePending(value: unknown, networkId: string): PendingOperation[] {
  return list(value).map((item) => {
    const data = record(item)
    const request = data.control == null ? operationRequest(data.request, networkId) : controlRequest(data.control, networkId)
    for (const key of ["operation_id", "target_node", "conversation_id", "session_id"] as const) {
      if (data[key] !== request[key]) throw new Error("待核对操作内容不一致。")
    }
    const identity = { operation_id: request.operation_id, target_node: request.target_node, conversation_id: request.conversation_id, session_id: request.session_id }
    return "expected_operation_id" in request ? { ...identity, control: request } : { ...identity, request }
  })
}
export function parsePendingMessages(value: unknown): PendingMessage[] {
  return list(value).map((item) => {
    const data = record(item)
    return { message_id: text(data.message_id), conversation_id: text(data.conversation_id), text: text(data.text), scheduler_node: text(data.scheduler_node), entry_node: text(data.entry_node), basis_heads: texts(data.basis_heads), report_results: data.report_results == null ? undefined : flag(data.report_results) }
  })
}
export function parsePendingConversation(value: unknown): PendingConversation | null {
  if (value === null) return null
  return conversation(value)
}
export function parsePendingTasks(value: unknown): PendingTask[] {
  return list(value).map((item) => {
    const data = record(item)
    return { conversation_id: text(data.conversation_id), task_id: text(data.task_id), title: text(data.title), goal: text(data.goal), acceptance: text(data.acceptance) }
  })
}
export function frontierMap(value: unknown) {
  const next: Record<string, string[]> = {}
  for (const [key, heads] of Object.entries(record(value))) next[key] = texts(heads)
  return next
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
