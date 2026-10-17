import type {
  ConversationPayload,
  ConversationSummary,
  NodeInfo,
  NotificationRecord,
  OperationRecord,
  PendingOperation,
  SessionRecord,
  TaskRecord,
} from "./types"

function record(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" ? (value as Record<string, unknown>) : null
}

function text(value: unknown) {
  return typeof value === "string" ? value : undefined
}

function number(value: unknown) {
  return typeof value === "number" ? value : undefined
}

function flag(value: unknown) {
  return typeof value === "boolean" ? value : undefined
}

function texts(value: unknown) {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : undefined
}

export function parseNodes(value: unknown): NodeInfo[] {
  const nodes = record(value)?.nodes
  if (!Array.isArray(nodes)) return []
  return nodes.flatMap((item) => {
    const node = record(item)
    const nodeId = text(node?.node_id)
    if (!nodeId) return []
    const capabilities = record(node?.capabilities)
    return [{
      node_id: nodeId,
      local: flag(node?.local),
      online: flag(node?.online),
      roles: texts(node?.roles),
      capabilities: capabilities
        ? {
            manager: flag(capabilities.manager),
            executors: texts(capabilities.executors),
            roots: texts(capabilities.roots),
          }
        : undefined,
    }]
  })
}

export function parseConversations(value: unknown): ConversationSummary[] {
  const conversations = record(value)?.conversations
  if (!Array.isArray(conversations)) return []
  return conversations.flatMap((item) => {
    const conversation = record(item)
    const conversationId = text(conversation?.conversation_id)
    if (!conversationId) return []
    return [{ conversation_id: conversationId, title: text(conversation?.title) }]
  })
}

export function parseNotifications(value: unknown): NotificationRecord[] {
  const notifications = record(value)?.notifications
  if (!Array.isArray(notifications)) return []
  return notifications.flatMap((item) => {
    const notification = record(item)
    const notificationId = text(notification?.notification_id)
    const conversationId = text(notification?.conversation_id)
    if (!notificationId || !conversationId) return []
    return [{
      notification_id: notificationId,
      conversation_id: conversationId,
      kind: text(notification?.kind),
      text: text(notification?.text),
    }]
  })
}

export function parseConversation(value: unknown): ConversationPayload {
  const data = record(value)
  const conversation = record(data?.conversation)
  const missing = record(data?.missing)
  return {
    conversation: conversation ? { title: text(conversation.title) } : undefined,
    heads: texts(data?.heads),
    missing: missing
      ? { events: missing.events, blobs: missing.blobs, conflicts: missing.conflicts }
      : undefined,
    tasks: Array.isArray(data?.tasks) ? data.tasks.flatMap(parseTask) : [],
    sessions: Array.isArray(data?.sessions) ? data.sessions.flatMap(parseSession) : [],
    operations: Array.isArray(data?.operations) ? data.operations.flatMap(parseOperation) : [],
  }
}

function parseTask(value: unknown): TaskRecord[] {
  const task = record(value)
  const taskId = text(task?.task_id)
  if (!taskId) return []
  return [{
    task_id: taskId,
    title: text(task?.title),
    goal: text(task?.goal),
    acceptance: text(task?.acceptance),
    detail: text(task?.detail),
    status: text(task?.status),
  }]
}

function parseSession(value: unknown): SessionRecord[] {
  const session = record(value)
  const target = text(session?.target_node)
  const sessionId = text(session?.session_id)
  if (!target || !sessionId) return []
  return [{
    target_node: target,
    session_id: sessionId,
    executor: text(session?.executor),
    state: text(session?.state),
    cwd: text(session?.cwd),
    revision: number(session?.revision),
    native_session_id: text(session?.native_session_id),
  }]
}

function parseOperation(value: unknown): OperationRecord[] {
  const operation = record(value)
  if (!operation) return []
  const request = record(operation.request)
  return [{
    state: text(operation.state),
    request: request
      ? { operation_id: text(request.operation_id), target_node: text(request.target_node) }
      : undefined,
    raw: value,
  }]
}

export function parsePending(value: unknown): PendingOperation[] {
  if (!Array.isArray(value)) return []
  return value.flatMap((item) => {
    const operation = record(item)
    const operationId = text(operation?.operation_id)
    const target = text(operation?.target_node)
    const conversationId = text(operation?.conversation_id)
    if (!operationId || !target || !conversationId) return []
    return [{
      operation_id: operationId,
      target_node: target,
      conversation_id: conversationId,
      session_id: text(operation?.session_id),
    }]
  })
}

export function parseInfo(value: unknown) {
  const info = record(value)
  return { node_id: text(info?.node_id), version: text(info?.version) }
}

export function parseHeads(value: unknown) {
  return texts(record(value)?.heads) ?? []
}

export function frontierMap(value: unknown) {
  const map = record(value) ?? {}
  const next: Record<string, string[]> = {}
  for (const [key, heads] of Object.entries(map)) {
    if (Array.isArray(heads) && heads.every((head) => typeof head === "string")) next[key] = heads
  }
  return next
}
