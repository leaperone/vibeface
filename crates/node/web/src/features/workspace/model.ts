import type { PendingMessage, TranscriptMessage } from "../chat/types"
import type {
  ConversationPayload, ConversationSummary, NodeInfo, NotificationRecord, OperationRecord,
  OperationRequest, OperateAction, PendingOperation, SessionRecord, TaskRecord,
  PendingBinding, PendingConversation, PendingTask,
} from "./types"
import type { AuthorizedRequest } from "../../lib/node"

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
  return { message_id: text(data.message_id), role: text(data.role), text: text(data.text), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms) }
}
export function parseTask(value: unknown): TaskRecord {
  const data = record(value)
  return { task_id: text(data.task_id), title: text(data.title), goal: text(data.goal), acceptance: text(data.acceptance), detail: text(data.detail), status: text(data.status), task_creation: record(data.task_creation), authority_public_key: text(data.authority_public_key), binding: data.binding === null ? null : record(data.binding) }
}
export function parseSession(value: unknown): SessionRecord {
  const data = record(value)
  return { target_node: text(data.target_node), session_id: text(data.session_id), executor: text(data.executor), state: text(data.state), cwd: text(data.cwd), revision: number(data.revision), native_session_id: data.native_session_id == null ? undefined : text(data.native_session_id), task_id: text(data.task_id), binding: record(data.binding), task_creation: record(data.task_creation), authority_public_key: text(data.authority_public_key) }
}
export function parseOperation(value: unknown): OperationRecord {
  const data = record(value), request = record(data.request)
  const state = text(data.state)
  if (!["accepted", "running", "unknown", "succeeded", "failed", "rejected"].includes(state)) throw new Error("未知的操作状态。")
  return { state, request: operationRequest(request, text(request.space_id)), raw: value }
}
export function parseConversation(value: unknown, expectedId?: string): ConversationPayload {
  const data = record(value), missing = record(data.missing)
  const owner = data.conversation === null ? null : conversation(data.conversation)
  if (expectedId && owner?.conversation_id !== expectedId) throw new Error("节点返回了其他会话，已暂停操作。")
  return {
    conversation: owner, messages: list(data.messages).map(message), heads: texts(data.heads),
    missing: { events: texts(missing.events), blobs: list(missing.blobs), conflicts: texts(missing.conflicts) },
    tasks: list(data.tasks).map(parseTask), sessions: list(data.sessions).map(parseSession), operations: list(data.operations).map(parseOperation),
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

function operationIntent(value: unknown, spaceId: string): Omit<OperationRequest, "binding"> {
  const data = record(value)
  if (data.space_id !== spaceId || typeof data.task_id !== "string" || !data.task_id) throw new Error("待核对操作缺少工单或属于其他空间。")
  return {
    operation_id: text(data.operation_id), space_id: spaceId, conversation_id: text(data.conversation_id), task_id: data.task_id,
    target_node: text(data.target_node), session_id: text(data.session_id), expected_revision: number(data.expected_revision),
    basis_heads: texts(data.basis_heads), resources: texts(data.resources), action: parseAction(data.action),
  }
}
function operationRequest(value: unknown, spaceId: string): OperationRequest {
  const data = record(value), request = operationIntent(data, spaceId), proof = record(data.binding), binding = record(proof.binding)
  if (!text(proof.signature)) throw new Error("待核对操作缺少绑定签名。")
  for (const key of ["space_id", "conversation_id", "task_id", "target_node", "session_id"] as const) {
    if (binding[key] !== request[key]) throw new Error("待核对操作与工单绑定不一致。")
  }
  return { ...request, binding: proof }
}
export function parsePending(value: unknown, spaceId: string): PendingOperation[] {
  return list(value).map((item) => {
    const data = record(item), request = operationRequest(data.request, spaceId)
    for (const key of ["space_id", "operation_id", "target_node", "conversation_id", "session_id"] as const) {
      if (data[key] !== request[key]) throw new Error("待核对操作内容不一致。")
    }
    return { space_id: request.space_id, operation_id: request.operation_id, target_node: request.target_node, conversation_id: request.conversation_id, session_id: request.session_id, request }
  })
}
export function parsePendingMessages(value: unknown): PendingMessage[] {
  return list(value).map((item) => {
    const data = record(item)
    return { message_id: text(data.message_id), conversation_id: text(data.conversation_id), text: text(data.text), scheduler_node: text(data.scheduler_node), entry_node: text(data.entry_node), basis_heads: texts(data.basis_heads) }
  })
}
export function parsePendingConversation(value: unknown): PendingConversation | null {
  if (value === null) return null
  return conversation(value)
}
export function parsePendingTasks(value: unknown): PendingTask[] {
  return list(value).map((item) => {
    const data = record(item)
    const attempts = list(data.attempts).map(parseAuthorizedRequest)
    if (attempts.length > 8) throw new Error("工单签名尝试超出上限，请保留记录并核对。")
    return { conversation_id: text(data.conversation_id), task_id: text(data.task_id), title: text(data.title), goal: text(data.goal), acceptance: text(data.acceptance), auto_followup: flag(data.auto_followup), attempts }
  })
}
function parseAuthorizedRequest(value: unknown): AuthorizedRequest {
  const data = record(value)
  return { body: record(data.body), invocation: record(data.invocation) }
}
export function parsePendingBindings(value: unknown, spaceId: string): PendingBinding[] {
  return list(value).map((item) => {
    const data = record(item), request = operationIntent(data.request, spaceId), task = parseTask(data.task)
    if (request.action.type !== "open" || request.task_id !== task.task_id) throw new Error("待核对绑定与工单不一致。")
    return { request: { ...request, action: request.action }, task, attempt: data.attempt === null ? null : parseAuthorizedRequest(data.attempt) }
  })
}
export function frontierMap(value: unknown) {
  const next: Record<string, string[]> = {}
  for (const [key, heads] of Object.entries(record(value))) next[key] = texts(heads)
  return next
}
