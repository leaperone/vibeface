import type { TranscriptMessage } from "../chat/types"
import type { ReplyStatus } from "../chat/types"
import type {
  AnswerResult, ConversationPayload, ConversationSummary, NodeInfo, NotificationRecord, Outlet, Question, QuestionSubject, SessionHandoff, Severity, TriageSettings,
  SchedulerSnapshot, ConflictVersion, WriteStatus, WorkRecord, WorkState, WorkOutcome, BotRecord, CreateBotRequest, PendingBot, PendingCheck, CoordinationCheck,
  AssistantChange, AssistantChangeRecord, AssistantRoutine, AssistantSkill, AssistantView, FieldEdit,
} from "./types"
import { answerId } from "./questions.ts"

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
      local: flag(data.local), online: flag(data.online), revoked: flag(data.revoked),
      capabilities: capabilities ? { manager: capabilities.manager == null ? null : text(capabilities.manager), relay: capabilities.relay === true, roots: texts(capabilities.roots) } : undefined,
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
  return { message_id: text(data.message_id), role: text(data.role), text: text(data.text), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms), in_reply_to: optionalText(data.in_reply_to), status: data.status == null ? undefined : replyStatus(data.status), blobs: blobReferences(data.blobs), answers: optionalText(data.answers) }
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
function workState(value: unknown): WorkState {
  switch (value) {
    case "accepted": case "running": case "interrupting": case "finished": case "unverified": return value
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
    failure_reason: data.failure_reason == null ? null : text(data.failure_reason), thread_id: optionalText(data.thread_id),
  }
}
/** Requests this device still carries: an untaken dispatch shows as its own task row, a stop marks its work. */
function withDeliveries(works: WorkRecord[], value: unknown): WorkRecord[] {
  const rows = [...works]
  for (const item of list(value)) {
    const data = record(item), state = data.state, work_id = text(data.work_id), target_node = text(data.target_node)
    if (state === "dispatch_waiting" || state === "dispatch_rejected") {
      rows.push({ work_id, trigger: "", target_node, cwd: text(data.cwd), title: data.title == null ? "" : text(data.title), prompt: "", state, summary: "", outcome: null, result_text: "", failure_reason: data.reason == null ? null : text(data.reason) })
    } else if (state === "stop_waiting" || state === "stop_timed_out") {
      const index = rows.findIndex((row) => row.work_id === work_id)
      if (index >= 0) rows[index] = { ...rows[index], stop: state }
    } else throw new Error("送达状态无法识别。")
  }
  return rows
}
function coordinationCheck(value: unknown): CoordinationCheck {
  const data = record(value)
  return { check_id: text(data.check_id), conversation_id: text(data.conversation_id), trigger: text(data.trigger), attempt_id: text(data.attempt_id), detail: text(data.detail), process_groups: list(data.process_groups).map(number), created_at_ms: number(data.created_at_ms) }
}
/** One card per thing the person must settle: each unverified work, then each coordination process still alive. */
export function pendingChecks(data: Pick<ConversationPayload, "works" | "checks"> | null | undefined): PendingCheck[] {
  if (!data) return []
  return [
    ...data.works.filter((item) => item.state === "unverified").map((item): PendingCheck => ({ kind: "work", check_id: `work:${item.work_id}`, detail: item.failure_reason ?? "", work_id: item.work_id, target_node: item.target_node })),
    ...data.checks.map((check): PendingCheck => ({ kind: "process", check_id: check.check_id, detail: check.detail })),
  ]
}
export function parseConversation(value: unknown, expectedId?: string): ConversationPayload {
  const data = record(value)
  const owner = data.conversation === null ? null : conversation(data.conversation)
  if (expectedId && owner?.conversation_id !== expectedId) throw new Error("设备返回了别的对话，已暂停。")
  return {
    conversation: owner, messages: list(data.messages).map(message),
    works: withDeliveries(list(data.works).map(work), data.deliveries),
    scheduler: schedulerSnapshot(data.scheduler),
    checks: list(data.checks).map(coordinationCheck),
    local_node: text(data.local_node),
    changes: list(data.changes).map(changeRecord),
    assistant: assistantView(data.assistant),
    questions: list(data.questions).map(question),
    sessions: list(data.sessions).map(sessionHandoff),
  }
}
function edit(value: unknown): FieldEdit | undefined {
  if (value == null) return undefined
  const data = record(value)
  return { before: text(data.before), after: text(data.after) }
}
function skill(value: unknown): AssistantSkill {
  const data = record(value)
  return { name: text(data.name), description: text(data.description), body: text(data.body) }
}
function routine(value: unknown): AssistantRoutine {
  const data = record(value)
  const every = number(data.every_minutes)
  if (every === 0) throw new Error("例行任务的间隔不对。")
  return { name: text(data.name), prompt: text(data.prompt), first_run_at_ms: number(data.first_run_at_ms), every_minutes: every, enabled: flag(data.enabled) }
}
function assistantChange(value: unknown): AssistantChange {
  const data = record(value)
  if (data.kind === "profile") {
    const name = edit(data.name), instructions = edit(data.instructions)
    if (!name && !instructions) throw new Error("助手修改记录是空的。")
    return { kind: "profile", ...(name ? { name } : {}), ...(instructions ? { instructions } : {}) }
  }
  if (data.kind !== "skill" && data.kind !== "routine") throw new Error("助手修改记录无法识别。")
  if (data.before == null && data.after == null) throw new Error("助手修改记录是空的。")
  return data.kind === "skill"
    ? { kind: "skill", skill_id: text(data.skill_id), before: side(skill, data.before), after: side(skill, data.after) }
    : { kind: "routine", routine_id: text(data.routine_id), before: side(routine, data.before), after: side(routine, data.after) }
}
function side<T>(read: (value: unknown) => T, value: unknown): T | null { return value == null ? null : read(value) }
function changeRecord(value: unknown): AssistantChangeRecord {
  const data = record(value)
  return { change_id: text(data.change_id), cause: text(data.cause), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms), change: assistantChange(data.change) }
}
function assistantView(value: unknown): AssistantView {
  const data = record(value)
  return {
    skills: list(data.skills).map((item) => { const skill = record(item); return { skill_id: text(skill.skill_id), name: text(skill.name), description: text(skill.description) } }),
    routines: list(data.routines).map((item) => {
      const view = record(item)
      return { ...routine(view), routine_id: text(view.routine_id), next_run_at_ms: view.next_run_at_ms === null ? null : number(view.next_run_at_ms) }
    }),
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
/** The primary assistant's conversation: sessions no assistant started go there first. */
export function parsePrimary(value: unknown): string | null {
  const primary = record(value).primary
  return primary == null ? null : text(primary)
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
  if (value !== "info" && value !== "attention" && value !== "urgent") throw new Error("问题的紧急程度无法识别。")
  return value
}
function subject(value: unknown): QuestionSubject {
  const data = record(value)
  return { node: text(data.node), agent: text(data.agent), session_id: text(data.session_id), cwd: text(data.cwd), summary: text(data.summary), ...(data.request_id == null ? {} : { request_id: text(data.request_id) }) }
}
function question(value: unknown): Question {
  const data = record(value)
  const assistant = data.assistant == null ? null : record(data.assistant), answer = data.answer == null ? null : record(data.answer)
  return {
    question_id: text(data.question_id), conversation_id: data.conversation_id == null ? null : text(data.conversation_id),
    assistant: assistant && { bot_id: text(assistant.bot_id), name: text(assistant.name) },
    severity: severity(data.severity), text: text(data.text),
    choices: list(data.choices ?? []).map((item) => {
      const choice = record(item)
      return { label: text(choice.label), ...(choice.conversation_id == null ? {} : { conversation_id: text(choice.conversation_id) }) }
    }),
    about: subject(data.about), author: text(data.author), created_at_ms: number(data.created_at_ms),
    answer: answer && { message_id: text(answer.message_id), text: text(answer.text), author: text(answer.author), created_at_ms: number(answer.created_at_ms) },
  }
}
function sessionHandoff(value: unknown): SessionHandoff {
  const data = record(value)
  return { node: text(data.node), agent: text(data.agent), session_id: text(data.session_id), cwd: text(data.cwd), summary: text(data.summary), question_id: text(data.question_id), conversation_id: text(data.conversation_id), created_at_ms: number(data.created_at_ms) }
}
/** Every unanswered question across the network. */
export function parseQuestions(value: unknown): Question[] {
  return list(record(value).questions).map(question)
}
/** The answer that stands for this question, whether this browser's or one written first. */
export function parseAnswer(value: unknown, questionId: string): AnswerResult {
  const data = record(value), message = record(data.message), status = data.status
  if (status !== "answered" && status !== "already_answered") throw new Error("回答结果无法识别。")
  if (message.message_id !== answerId(questionId)) throw new Error("回答结果和这个问题对不上，请再查一次。")
  return { status, text: text(message.text) }
}
function outlet(value: unknown): Outlet {
  if (value !== "record" && value !== "both") throw new Error("提醒去向无法识别。")
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
