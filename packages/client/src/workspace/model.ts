import type { Attachment, TranscriptMessage } from "../chat/types.ts"
import type { ReplyStatus } from "../chat/types.ts"
import type {
  AnswerResult, CloseReason, ConversationPayload, ConversationSummary, NodeInfo, NotificationRecord, Outlet, Question, QuestionSubject, SessionHandoff, Severity, TriageSettings,
  SchedulerSnapshot, ConflictVersion, WriteStatus, WorkRecord, WorkState, WorkOutcome, BotRecord, CreateBotRequest, PendingBot, PendingCheck, CoordinationCheck,
  BotChange, BotChangeRecord, BotSchedule, BotSkill, BotSettings, SentBotMessage, FieldEdit, ModelEdit, BotModels, ConfirmReason, ConversationRecords, OpenWork, SkillSet, SteerOutcome,
} from "./types.ts"
import { answerId } from "./questions.ts"
import type { ProviderId } from "../lib/node.ts"

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
      capabilities: capabilities ? { manager: capabilities.manager == null ? null : text(capabilities.manager), manager_ready: capabilities.manager_ready === true, relay: capabilities.relay === true, roots: texts(capabilities.roots), codex_connected: capabilities.codex != null && record(capabilities.codex).connected === true } : undefined,
    }
  })
}
/** `ready` only when the bot's switch is on and its provider works; a switch alone leaves it `needs_setup`. */
export function botReadiness(node: NodeInfo | undefined): "ready" | "needs_setup" | null {
  if (!node || node.revoked || !node.online || !node.capabilities?.manager) return null
  return node.capabilities.manager_ready ? "ready" : "needs_setup"
}
export function parseNotifications(value: unknown): NotificationRecord[] {
  return list(record(value).notifications).map((item) => {
    const data = record(item)
    return { notification_id: text(data.notification_id), conversation_id: text(data.conversation_id), kind: text(data.kind), text: text(data.text) }
  })
}
function message(value: unknown): TranscriptMessage {
  const data = record(value)
  return { message_id: text(data.message_id), role: text(data.role), text: text(data.text), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms), in_reply_to: optionalText(data.in_reply_to), status: data.status == null ? undefined : replyStatus(data.status), attachments: attachments(data.attachments), answers: optionalText(data.answers), rank: optionalText(data.rank), sender: data.sender == null ? undefined : sender(data.sender) }
}
function sender(value: unknown) {
  const data = record(value)
  return { conversation_id: text(data.conversation_id), name: text(data.name) }
}
function replyStatus(value: unknown): ReplyStatus {
  switch (value) {
    case "completed": case "stopped": case "interrupted": case "failed": return value
    default: throw new Error("未知的回复状态。")
  }
}
function attachments(value: unknown) { return value == null ? undefined : list(value).map((item) => { const file = record(item); return { hash: text(file.hash), size: number(file.size), name: text(file.name) } }) }
function optionalText(value: unknown): string | undefined { return value == null ? undefined : text(value) }
function schedulerSnapshot(value: unknown): SchedulerSnapshot | null {
  if (value == null) return null
  const data = record(value), capabilities = data.capabilities == null ? null : record(data.capabilities), remote = data.remote == null ? null : record(data.remote)
  return {
    unanswered: list(data.unanswered).map(text),
    jobs: list(data.jobs).map((item) => { const job = record(item); return { trigger: text(job.trigger), state: text(job.state), message_id: optionalText(job.message_id), reply_message_id: optionalText(job.reply_message_id), error: optionalText(job.error), pending_check: job.pending_check === true, outcome: optionalText(job.outcome) as SteerOutcome | undefined, source_node: optionalText(job.source_node) } }),
    provider: optionalText(data.provider), model: optionalText(data.model), capabilities: capabilities ? { streaming: flag(capabilities.streaming), cancel: flag(capabilities.cancel), steer: flag(capabilities.steer) } : undefined,
    remote: remote ? { owner: text(remote.owner), reachable: flag(remote.reachable) } : undefined,
  }
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
function workKind(value: unknown): WorkRecord["kind"] {
  if (value == null) return undefined
  if (value !== "codex" && value !== "task") throw new Error("任务类型无法识别。")
  return value
}
function work(value: unknown): WorkRecord {
  const data = record(value)
  return {
    work_id: text(data.work_id), trigger: text(data.trigger), target_node: text(data.target_node), cwd: text(data.cwd), title: data.title == null ? "" : text(data.title), prompt: text(data.prompt),
    state: workState(data.state), summary: text(data.summary), outcome: workOutcome(data.outcome), result_text: text(data.result_text),
    failure_reason: data.failure_reason == null ? null : text(data.failure_reason), kind: workKind(data.kind), thread_id: optionalText(data.thread_id), turn_id: optionalText(data.turn_id),
  }
}
/** Requests this device still carries: an untaken dispatch shows as its own task row, a stop marks its work. */
export function withDeliveries(works: WorkRecord[], value: unknown): WorkRecord[] {
  const rows = [...works]
  for (const item of list(value)) {
    const data = record(item), state = data.state, work_id = text(data.work_id), target_node = text(data.target_node)
    if (state === "dispatch_waiting" || state === "dispatch_rejected" || state === "dispatch_unverified") {
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
/**
 * Why a work waits for the person, or null when its device can still report it. An unfinished work waits only once
 * its device is known to be offline, or its App Server disconnected for a Codex turn; an unknown device is not judged.
 * A task runs in its device's bot, which reports without the App Server, so only offline applies to it.
 */
export function confirmReason(item: WorkRecord, nodes: NodeInfo[]): ConfirmReason | null {
  if (item.state === "unverified") return "unverified"
  if (item.outcome || item.state === "finished" || item.state.startsWith("dispatch_")) return null
  const node = nodes.find((candidate) => candidate.device_id === item.target_node)
  if (!node || node.revoked) return null
  if (!node.online) return "offline"
  return item.kind === "codex" && node.capabilities?.codex_connected === false ? "disconnected" : null
}
/** One card per thing the person must settle: works waiting for confirmation, then each coordination process still alive. */
export function pendingChecks(data: Pick<ConversationPayload, "works" | "checks"> | null | undefined, nodes: NodeInfo[], conversationId: string): PendingCheck[] {
  if (!data) return []
  return [
    ...workChecks(data.works.map((item) => ({ ...item, conversation_id: conversationId })), nodes),
    ...data.checks.map((check): PendingCheck => ({ kind: "process", check_id: check.check_id, detail: check.detail })),
  ]
}
export type WorkCheck = Extract<PendingCheck, { kind: "work" }>
/** Each work waiting for confirmation; works one offline device or one disconnected Codex holds share one card, where the first of them stood. */
export function workChecks(works: OpenWork[], nodes: NodeInfo[]): PendingCheck[] {
  const checks = works.flatMap((item): WorkCheck[] => {
    const reason = confirmReason(item, nodes)
    return reason ? [{ kind: "work", check_id: `work:${item.work_id}`, detail: item.failure_reason ?? "", work_id: item.work_id, target_node: item.target_node, conversation_id: item.conversation_id, title: item.title, reason }] : []
  })
  const held = (check: WorkCheck) => check.reason !== "unverified" ? `device:${check.target_node}:${check.reason}` : null
  const grouped = new Map<string, WorkCheck[]>()
  for (const check of checks) { const key = held(check); if (key) grouped.set(key, [...grouped.get(key) ?? [], check]) }
  return checks.flatMap((check): PendingCheck[] => {
    const key = held(check), same = key ? grouped.get(key)! : [check]
    if (same.length < 2) return [check]
    if (same[0] !== check) return []
    return [{ kind: "device", check_id: key!, detail: "", target_node: check.target_node, reason: check.reason as "offline" | "disconnected", works: same }]
  })
}
export function parseOpenWorks(value: unknown): OpenWork[] {
  return list(record(value).works).map((item) => ({ ...work(item), conversation_id: text(record(item).conversation_id) }))
}
/** A pushed change: the records it touched, or null when it carries none. */
export function parseRecords(value: unknown): ConversationRecords {
  const data = record(value)
  const last = data.last_message == null ? null : record(data.last_message)
  const scheduler = schedulerSnapshot({ jobs: data.jobs, unanswered: data.unanswered })
  return {
    conversation_id: text(data.conversation_id), messages: list(data.messages).map(message), works: list(data.works).map(work),
    last_message: last && { text: text(last.text), role: text(last.role), created_at_ms: number(last.created_at_ms) },
    questions: list(data.questions).map(question), notifications: parseNotifications(data),
    deliveries: list(data.deliveries), checks: list(data.checks).map(coordinationCheck),
    jobs: scheduler?.jobs ?? [], unanswered: scheduler?.unanswered ?? [],
    sent: list(data.sent).map(sentBotMessage),
  }
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
    bot_settings: botSettings(data.bot_settings),
    questions: list(data.questions).map(question),
    sessions: list(data.sessions).map(sessionHandoff),
    sent: list(data.sent).map(sentBotMessage),
  }
}
function edit(value: unknown): FieldEdit | undefined {
  if (value == null) return undefined
  const data = record(value)
  return { before: text(data.before), after: text(data.after) }
}
const providers: readonly ProviderId[] = ["codex", "cursor"]
function provider(value: unknown): ProviderId {
  if (!providers.includes(value as ProviderId)) throw new Error("模型的 provider 无法识别。")
  return value as ProviderId
}
function modelEdit(value: unknown): ModelEdit | undefined {
  const field = edit(value)
  return field && { provider: provider(record(value).provider), ...field }
}
function botModels(value: unknown): BotModels {
  return Object.fromEntries(Object.entries(record(value)).map(([key, model]) => [provider(key), text(model)]))
}
function skillSet(value: unknown): SkillSet {
  return value == null ? null : list(value).map(text)
}
function skill(value: unknown): BotSkill {
  const data = record(value)
  return { name: text(data.name), description: text(data.description), body: text(data.body) }
}
function schedule(value: unknown): BotSchedule {
  const data = record(value)
  const every = data.every_minutes == null ? undefined : number(data.every_minutes)
  if (every === 0) throw new Error("定时任务的间隔不对。")
  return {
    name: text(data.name), prompt: text(data.prompt), first_run_at_ms: number(data.first_run_at_ms), enabled: flag(data.enabled),
    ...(every === undefined ? {} : { every_minutes: every }), ...(data.depth == null ? {} : { depth: number(data.depth) }),
  }
}
function botChange(value: unknown): BotChange {
  const data = record(value)
  if (data.kind === "profile") {
    const name = edit(data.name), instructions = edit(data.instructions), model = modelEdit(data.model)
    const skills = data.skills == null ? undefined : { before: skillSet(record(data.skills).before), after: skillSet(record(data.skills).after) }
    if (!name && !instructions && !model && !skills) throw new Error("Bot 修改记录是空的。")
    return { kind: "profile", ...(name ? { name } : {}), ...(instructions ? { instructions } : {}), ...(model ? { model } : {}), ...(skills ? { skills } : {}) }
  }
  if (data.before == null && data.after == null) throw new Error("Bot 修改记录是空的。")
  switch (data.kind) {
    case "skill": return { kind: "skill", skill_id: text(data.skill_id), before: side(skill, data.before), after: side(skill, data.after) }
    case "schedule": return { kind: "schedule", schedule_id: text(data.schedule_id), before: side(schedule, data.before), after: side(schedule, data.after) }
    case "memory": return { kind: "memory", memory_id: text(data.memory_id), before: side(text, data.before), after: side(text, data.after) }
    default: throw new Error("Bot 修改记录无法识别。")
  }
}
function side<T>(read: (value: unknown) => T, value: unknown): T | null { return value == null ? null : read(value) }
function changeRecord(value: unknown): BotChangeRecord {
  const data = record(value)
  const cause = data.cause == null ? null : record(data.cause)
  return { change_id: text(data.change_id), conversation_id: text(data.conversation_id), cause: cause && { conversation_id: text(cause.conversation_id), message_id: text(cause.message_id) }, event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms), change: botChange(data.change) }
}
function botSettings(value: unknown): BotSettings {
  const data = record(value)
  return {
    skills: list(data.skills).map((item) => { const skill = record(item); return { skill_id: text(skill.skill_id), name: text(skill.name), description: text(skill.description) } }),
    schedules: list(data.schedules).map((item) => {
      const view = record(item)
      return { ...schedule(view), schedule_id: text(view.schedule_id), next_run_at_ms: view.next_run_at_ms == null ? null : number(view.next_run_at_ms) }
    }),
    memory: list(data.memory).map((item) => { const entry = record(item); return { memory_id: text(entry.memory_id), text: text(entry.text) } }),
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

/** The largest attachment the entry accepts; an empty file is refused too. */
export const attachmentLimit = 64 * 1024 * 1024

/** The name a message signs, as the Node checks it: no path separators or control characters, at most 255 UTF-8 bytes. */
export function attachmentName(raw: string) {
  const encoder = new TextEncoder()
  let name = "", bytes = 0
  for (const char of raw.replace(/[/\\\p{Cc}\p{Cs}]/gu, "")) {
    bytes += encoder.encode(char).length
    if (bytes > 255) break
    name += char
  }
  return name || "file"
}

/** The Node accepts attachments ordered by content, once each; the first name given for the same file stays. */
export function signedOrder(files: readonly Attachment[]) {
  return files.filter((file, index) => files.findIndex((other) => other.hash === file.hash) === index)
    .sort((a, b) => a.hash < b.hash ? -1 : a.hash > b.hash ? 1 : 0)
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
  const data = record(value), last = data.last_message == null ? null : record(data.last_message)
  return { ...parseCreateBot(value), models: botModels(data.models), skills: skillSet(data.skills), event_hash: text(data.event_hash), last_message: last && { text: text(last.text), role: text(last.role), created_at_ms: number(last.created_at_ms) } }
}
export function parseBots(value: unknown): BotRecord[] {
  return list(record(value).bots).map(parseBot)
}
/** The primary bot's conversation: sessions no bot started go there first. */
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
function closeReason(value: unknown): CloseReason {
  if (value !== "request_lost") throw new Error("问题关闭的原因无法识别。")
  return value
}
function question(value: unknown): Question {
  const data = record(value)
  const bot = data.bot == null ? null : record(data.bot), answer = data.answer == null ? null : record(data.answer)
  return {
    question_id: text(data.question_id), conversation_id: text(data.conversation_id),
    bot: bot && { bot_id: text(bot.bot_id), name: text(bot.name) },
    severity: severity(data.severity), text: text(data.text),
    choices: list(data.choices ?? []).map((item) => ({ label: text(record(item).label) })),
    about: subject(data.about), author: text(data.author), created_at_ms: number(data.created_at_ms),
    answer: answer && { message_id: text(answer.message_id), text: text(answer.text), author: text(answer.author), created_at_ms: number(answer.created_at_ms) },
    closed: data.closed == null ? null : closeReason(data.closed),
  }
}
function sentBotMessage(value: unknown): SentBotMessage {
  const data = record(value)
  return { message_id: text(data.message_id), to: text(data.to), text: text(data.text), created_at_ms: number(data.created_at_ms) }
}
function sessionHandoff(value: unknown): SessionHandoff {
  const data = record(value)
  return { node: text(data.node), agent: text(data.agent), session_id: text(data.session_id), cwd: text(data.cwd), summary: text(data.summary), muted: data.muted === true, created_at_ms: number(data.created_at_ms) }
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
    codex_sessions: data.codex_sessions === null ? null : text(data.codex_sessions),
    webhook_secret_set: flag(data.webhook_secret_set), decision_url: text(data.decision_url), device_id: text(data.device_id),
    decision_tokens: list(data.decision_tokens).map((item) => { const token = record(item); return { token_id: text(token.token_id), label: text(token.label), created_ms: number(token.created_ms) } }),
    outbox: { pending: number(outbox.pending), delivered: number(outbox.delivered), dead: number(outbox.dead) },
  }
}
