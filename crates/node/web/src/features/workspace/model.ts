import type { PendingMessage, TranscriptMessage } from "../chat/types"
import type {
  ConversationPayload, ConversationSummary, NodeInfo, NotificationRecord, OperationRecord,
  PendingOperation,
  FollowupMode, HumanRequest, SchedulerSnapshot, HumanQuestion, ExecutionActivityRecord, ExecutionControlRequest, BotRecord, CreateBotRequest, PendingBot,
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
    if (data.peer_id !== undefined && data.node_id !== undefined && data.peer_id !== data.node_id) throw new Error("设备编号不一致。")
    return {
      peer_id: text(data.peer_id ?? data.node_id),
      name: typeof data.name === "string" ? data.name : "",
      local: flag(data.local), online: flag(data.online), revoked: flag(data.revoked), roles: texts(data.roles),
      capabilities: capabilities ? { manager: capabilities.manager === null ? null : text(capabilities.manager), executors: texts(capabilities.executors), roots: texts(capabilities.roots) } : undefined,
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
function schedulerSnapshot(value: unknown): SchedulerSnapshot | null {
  if (value == null) return null
  const data = record(value), capabilities = data.capabilities == null ? null : record(data.capabilities)
  return { jobs: list(data.jobs).map((item) => { const job = record(item); return { trigger: text(job.trigger), state: text(job.state), kind: text(job.kind), sequence: number(job.sequence), message_id: optionalText(job.message_id), target_node: optionalText(job.target_node), reply_message_id: optionalText(job.reply_message_id), read_only: job.read_only == null ? undefined : flag(job.read_only), error: optionalText(job.error), delivery_error: optionalText(job.delivery_error) } }), provider: optionalText(data.provider), model: optionalText(data.model), capabilities: capabilities ? { streaming: flag(capabilities.streaming), cancel: flag(capabilities.cancel), steer: flag(capabilities.steer) } : undefined, write_frozen: data.write_frozen == null ? undefined : flag(data.write_frozen) }
}
export function hasQueuedMessage(scheduler: SchedulerSnapshot | null, pending: PendingMessage): boolean {
  return scheduler?.jobs.some((job) => job.message_id === pending.message_id && typeof job.target_node === "string" && job.target_node.length > 0 && (pending.scheduler_node === null || job.target_node === pending.scheduler_node) && ["queued", "claimed", "running", "cancelling", "completed", "cancelled", "failed", "unknown"].includes(job.state)) ?? false
}
export function parseOperation(value: unknown): OperationRecord {
  const data = record(value), request = record(data.request)
  const state = text(data.state)
  if (!["accepted", "running", "unknown", "succeeded", "failed", "rejected"].includes(state)) throw new Error("未知的操作状态。")
  return { state, request: { operation_id: text(request.operation_id), target_node: text(request.target_node), conversation_id: text(request.conversation_id), session_id: request.session_id === null ? null : text(request.session_id) }, raw: value }
}
function execution(value: unknown): ExecutionActivityRecord {
  const data = record(value)
  const state = text(data.state)
  if (!["accepted", "running", "unknown", "succeeded", "failed", "rejected"].includes(state)) throw new Error("未知的工作状态。")
  if (data.freshness !== "local_ledger" && data.freshness !== "cached" && data.freshness !== "unknown") throw new Error("未知的执行状态读取来源。")
  const goal = data.goal == null ? null : record(data.goal)
  return {
    operation_id: text(data.operation_id), target_node: text(data.target_node), session_id: data.session_id === null ? null : text(data.session_id),
    revision: number(data.revision), title: text(data.title), state, session_state: text(data.session_state),
    capabilities: texts(data.capabilities), pending_requests: list(data.pending_requests).map(humanRequest), freshness: data.freshness,
    checked_at_ms: data.checked_at_ms == null ? undefined : number(data.checked_at_ms), latest_progress: optionalText(data.latest_progress),
    detail: text(data.detail), blobs: blobReferences(data.blobs) ?? [], source_trigger: optionalText(data.source_trigger), source_message_id: optionalText(data.source_message_id),
    goal: goal ? { task_id: text(goal.task_id), title: text(goal.title), acceptance: text(goal.acceptance), status: text(goal.status), followup_mode: parseFollowupMode(goal.followup_mode), followup_version: number(goal.followup_version), followup_scheduler_node: optionalText(goal.followup_scheduler_node) } : undefined,
  }
}
export function parseConversation(value: unknown, expectedId?: string): ConversationPayload {
  const data = record(value), missing = record(data.missing)
  const owner = data.conversation === null ? null : conversation(data.conversation)
  if (expectedId && owner?.conversation_id !== expectedId) throw new Error("节点返回了其他会话，已暂停操作。")
  return {
    conversation: owner, messages: list(data.messages).map(message), heads: texts(data.heads),
    missing: { events: texts(missing.events), blobs: list(missing.blobs), conflicts: texts(missing.conflicts) },
    operations: list(data.operations).map(parseOperation),
    scheduler: schedulerSnapshot(data.scheduler),
    executions: list(data.executions).map(execution),
  }
}
function executionControlRequest(value: unknown): ExecutionControlRequest {
  const data = record(value), action = record(data.action)
  const request = { conversation_id: text(data.conversation_id), operation_id: text(data.operation_id), expected_operation_id: text(data.expected_operation_id), expected_revision: number(data.expected_revision) }
  switch (action.type) {
    case "interrupt": return { ...request, action: { type: "interrupt" } }
    case "steer": return { ...request, action: { type: "steer", text: text(action.text) } }
    case "respond_request":
      if (!("response" in action)) throw new Error("待核对审批缺少本人决定。")
      return { ...request, action: { type: "respond_request", request_id: typeof action.request_id === "string" ? action.request_id : number(action.request_id), response: action.response } }
    default: throw new Error("未知的待核对控制。")
  }
}
export function parsePending(value: unknown): PendingOperation[] {
  return list(value).map((item) => {
    const data = record(item)
    const identity = { operation_id: text(data.operation_id), target_node: text(data.target_node), conversation_id: text(data.conversation_id), session_id: data.session_id === null ? null : text(data.session_id) }
    const request = executionControlRequest(data.execution_control)
    if (identity.operation_id !== request.operation_id || identity.conversation_id !== request.conversation_id) throw new Error("待核对控制内容不一致。")
    return { ...identity, execution_control: request }
  })
}
export function parsePendingMessages(value: unknown): PendingMessage[] {
  return list(value).map((item) => {
    const data = record(item)
    return { message_id: text(data.message_id), conversation_id: text(data.conversation_id), text: text(data.text), scheduler_node: data.scheduler_node == null ? null : text(data.scheduler_node), entry_node: text(data.entry_node), basis_heads: texts(data.basis_heads), report_results: data.report_results == null ? undefined : flag(data.report_results), rejected: data.rejected == null ? undefined : { entry_node: text(record(data.rejected).entry_node) } }
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
