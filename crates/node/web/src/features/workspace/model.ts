import type { PendingMessage, TranscriptMessage } from "../chat/types"
import type {
  AgentTaskRecord, ConversationPayload, ConversationSummary, NodeInfo, NotificationRecord,
  HumanRequest, SchedulerSnapshot, HumanQuestion, NativeSessionRecord, AgentTaskState, NativeSessionState, BotRecord, CreateBotRequest, PendingBot,
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
      peer_id: text(data.peer_id),
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
  return { message_id: text(data.message_id), role: text(data.role), text: text(data.text), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms), blobs: blobReferences(data.blobs) }
}
function blobReferences(value: unknown) { return value == null ? undefined : list(value).map((item) => { const blob = record(item); return { hash: text(blob.hash), size: number(blob.size) } }) }
function optionalText(value: unknown): string | undefined { return value == null ? undefined : text(value) }
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
function agentTaskState(value: unknown): AgentTaskState {
  switch (value) {
    case "pending_delivery": case "queued": case "claimed": case "accepted": case "running": case "cancelling":
    case "completed": case "failed": case "cancelled": case "interrupted": case "unknown": return value
    default: throw new Error("未知的任务状态。")
  }
}
function nativeSessionState(value: unknown): NativeSessionState {
  switch (value) {
    case "running": case "waiting": case "interrupting": case "completed": case "failed": case "interrupted": case "unknown": return value
    default: throw new Error("未知的原生工作状态。")
  }
}
function agentTask(value: unknown): AgentTaskRecord {
  const data = record(value)
  return {
    task_ref: text(data.task_ref), source_node: text(data.source_node), target_node: text(data.target_node),
    origin_trigger: text(data.origin_trigger), state: agentTaskState(data.state), text: text(data.text), detail: text(data.detail),
  }
}
function nativeSession(value: unknown): NativeSessionRecord {
  const data = record(value)
  const freshness = data.freshness
  if (freshness !== "observed" && freshness !== "cached") throw new Error("未知的原生进度读取来源。")
  return {
    work_id: text(data.work_id), task_ref: data.task_ref === null ? null : text(data.task_ref),
    target_node: text(data.target_node), thread_id: text(data.thread_id), turn_id: data.turn_id === null ? null : text(data.turn_id),
    state: nativeSessionState(data.state), freshness, can_control: flag(data.can_control),
    detail: text(data.detail), pending_requests: list(data.pending_requests).map(humanRequest),
  }
}
export function parseConversation(value: unknown, expectedId?: string): ConversationPayload {
  const data = record(value), missing = record(data.missing)
  const owner = data.conversation === null ? null : conversation(data.conversation)
  if (expectedId && owner?.conversation_id !== expectedId) throw new Error("节点返回了其他会话，已暂停操作。")
  return {
    conversation: owner, messages: list(data.messages).map(message), heads: texts(data.heads),
    missing: { events: texts(missing.events), blobs: list(missing.blobs), conflicts: texts(missing.conflicts) },
    agent_tasks: list(data.agent_tasks).map(agentTask),
    native_sessions: list(data.native_sessions).map(nativeSession),
    observation_targets: texts(data.observation_targets),
    scheduler: schedulerSnapshot(data.scheduler),
  }
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
