import type { Attachment, PendingMessage, TranscriptMessage } from "../chat/types.ts"
import type { OutboxItem } from "../lib/outbox.ts"
import { array, boolean, isRecord, isSafeInteger, isText, parseProviderReadiness } from "../lib/node.ts"
import { providerStages, type ProviderStage, type TurnRuntime } from "./types.ts"
import type { ReplyStatus } from "../chat/types.ts"
import type {
  AnswerResult, BotReadiness, CloseReason, ConversationPayload, ConversationSummary, NodeInfo, NotificationRecord, Outlet, Question, QuestionChoice, QuestionSubject, SessionHandoff, Severity, TriageSettings,
  SchedulerSnapshot, ConflictVersion, WriteStatus, WorkRecord, WorkState, DispatchState, WorkOutcome, BotRecord, CreateBotRequest, PendingBot, PendingCheck, CoordinationCheck,
  BotChange, BotChangeRecord, Overwritten, BotSchedule, BotSkill, BotSettings, SentBotMessage, FieldEdit, ModelEdit, BotModels, ConfirmReason, ConversationRecords, OpenWork, OpenWorkCard, WorkCard, SkillSet, SteerOutcome, Busy,
  WorkPhase, WorkDetail, OverviewBot, OverviewNode, OverviewConsistency, OverviewTaskState, OverviewTask, OverviewQuestion, OverviewJob, OverviewCoverage, WorkspaceOverview, OverviewDeliveryState, OverviewRelation, OverviewConversation, OverviewSession,
  AgentLane, QueueHoldReason, ContextProjection, ManagerJob, ScopeJob, SentCoverage, MemoryConflict,
  InputTarget, InputIntent, InputReceipt, InputControlAction, QueuedInput, ScopeContext, SessionScope, WorkspaceChannel, WorkspaceTask, WorkspaceAssignment, WorkspaceCommand, WorkspaceCommandResult, WorkspaceCommandNotSaved, WorkspaceDomainState, WorkspaceHistory, WorkspaceHistoryBody, WorkspaceInputBody, WorkspaceSpeaker,
} from "./types.ts"
import { parseMemoryEntry, parseMemorySyncConflicts } from "./memory.ts"
import { answerId, questionOpen } from "./questions.ts"
import { providerIds, type ProviderId } from "../lib/node.ts"
import { failureKinds, type FailureKind } from "../lib/errors.ts"
import { EYES, HUES, OUTLINES, type Avatar } from "../avatar/face.ts"

export function inputText(value: string, label: string, maxBytes: number): string {
  const normalized = value.trim()
  if (!normalized) throw new Error(`${label}不能为空。`)
  if (new TextEncoder().encode(normalized).length > maxBytes) throw new Error(`${label}不能超过 ${maxBytes} 字节。`)
  return normalized
}

export function record(value: unknown): Record<string, unknown> {
  if (!isRecord(value)) throw new Error("设备返回的内容格式不对。")
  return value
}
function text(value: unknown): string {
  if (!isText(value)) throw new Error("设备返回的内容缺少文字。")
  return value
}
function number(value: unknown): number {
  if (!isSafeInteger(value) || value < 0) throw new Error("设备返回的数字不对。")
  return value
}
function flag(value: unknown): boolean { return boolean(value, "设备返回的内容缺少开关值。") }
function list(value: unknown): unknown[] { return array(value, "设备返回的内容缺少列表。") }
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
      capabilities: capabilities ? { manager: capabilities.manager == null ? null : text(capabilities.manager), manager_state: capabilities.manager == null ? null : readiness(capabilities.manager_state), manager_readiness: capabilities.manager_readiness == null ? null : parseProviderReadiness(capabilities.manager_readiness), relay: capabilities.relay === true, roots: texts(capabilities.roots), codex_connected: capabilities.codex != null && record(capabilities.codex).connected === true } : undefined,
    }
  })
}
function readiness(value: unknown): BotReadiness {
  if (value === "ready" || value === "starting" || value === "unverified" || value === "needs_setup") return value
  throw new Error("Bot 可用性状态无法识别。")
}
export function botReadiness(node: NodeInfo | undefined): BotReadiness | null {
  if (!node || node.revoked || !node.online || !node.capabilities?.manager) return null
  return node.capabilities.manager_state
}
export function parseNotifications(value: unknown): NotificationRecord[] {
  return list(record(value).notifications).map((item) => {
    const data = record(item)
    return { notification_id: text(data.notification_id), conversation_id: text(data.conversation_id), kind: text(data.kind), text: text(data.text), ...(data.target == null ? {} : { target: parseInputTarget(data.target) }) }
  })
}
function message(value: unknown): TranscriptMessage {
  const data = record(value)
  return { message_id: text(data.message_id), role: text(data.role), text: text(data.text), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms), in_reply_to: optionalText(data.in_reply_to), status: data.status == null ? undefined : replyStatus(data.status), ...failure(data.failure), attachments: attachments(data.attachments), answers: optionalText(data.answers), rank: optionalText(data.rank), sender: data.sender == null ? undefined : sender(data.sender), ...(data.target == null ? {} : { target: parseInputTarget(data.target) }) }
}
export const mainMessage = (message: Pick<TranscriptMessage, 'target'>) => message.target === undefined || message.target.kind === 'bot_main'
export const mainQuestion = (question: Pick<Question, 'target'>) => question.target?.kind === 'bot_main'
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
function failureKind(value: unknown): FailureKind | undefined {
  if (value == null) return undefined
  if (!failureKinds.includes(value as FailureKind)) throw new Error("失败原因无法识别。")
  return value as FailureKind
}
/** Present only when the record names one, so records without it keep their shape. */
function failure(value: unknown): { failure?: FailureKind } {
  const kind = failureKind(value)
  return kind ? { failure: kind } : {}
}
function attachments(value: unknown) { return value == null ? undefined : list(value).map((item) => { const file = record(item); return { hash: text(file.hash), size: number(file.size), name: text(file.name) } }) }
function optionalText(value: unknown): string | undefined { return value == null ? undefined : text(value) }
function optionalNumber(value: unknown): number | undefined { return value == null ? undefined : number(value) }
export function parseInputTarget(value: unknown): InputTarget {
  const data = record(value)
  switch (data.kind) {
    case 'bot_main': return { kind: data.kind, bot_id: text(data.bot_id) }
    case 'task': return { kind: data.kind, task_id: text(data.task_id) }
    case 'work': return { kind: data.kind, work_id: text(data.work_id) }
    case 'routine': return { kind: data.kind, bot_id: text(data.bot_id), trigger: text(data.trigger) }
    case 'channel_member': return { kind: data.kind, channel_id: text(data.channel_id), bot_id: text(data.bot_id) }
    case 'channel_task': return { kind: data.kind, channel_id: text(data.channel_id), task_id: text(data.task_id), bot_id: text(data.bot_id) }
    default: throw new Error('输入目标无法识别。')
  }
}
export function parseInputIntent(value: unknown): InputIntent {
  if (value === 'continue' || value === 'new_task' || value === 'defer') return value
  throw new Error('无法识别这条输入要如何处理。')
}
function workspaceInputFields(value: unknown): Omit<WorkspaceInputBody, 'type'> {
  const data = record(value), sequence = number(data.sequence)
  if (!sequence) throw new Error('输入顺序必须为正整数。')
  return { message_id: text(data.message_id), target: parseInputTarget(data.target), intent: parseInputIntent(data.intent), sequence, expected_revision: optionalNumber(data.expected_revision), text: text(data.text), attachments: attachments(data.attachments), report_results: data.report_results == null ? undefined : flag(data.report_results) }
}
function queuedInput(value: unknown): QueuedInput {
  const data = record(value)
  return { input: workspaceInputFields(data.input), conversation_id: text(data.conversation_id), source_event: text(data.source_event), control_event: text(data.control_event), effective_order: number(data.effective_order), deferred: flag(data.deferred), cancelled: flag(data.cancelled) }
}
function inputControlAction(value: unknown): InputControlAction {
  const data = record(value)
  switch (data.kind) {
    case 'release': case 'cancel': return { kind: data.kind }
    case 'edit': return { kind: data.kind, text: text(data.text), attachments: attachments(data.attachments) ?? [] }
    case 'move': return { kind: data.kind, before_message_id: optionalText(data.before_message_id) }
    default: throw new Error('输入队列操作无法识别。')
  }
}
export function parseInputReceipt(value: unknown): InputReceipt {
  const data = record(value), stage = inputStage(data.stage), retrySafe = flag(data.retry_safe)
  const controlEvent = data.control_event === null ? null : text(data.control_event)
  if (controlEvent !== null && !/^[a-f0-9]{64}$/i.test(controlEvent)) throw new Error('输入回执的操作引用格式不对。')
  if (retrySafe && controlEvent === null) throw new Error('输入回执缺少安全重试所需的操作引用。')
  if (retrySafe && (stage === 'unknown' || stage === 'submitting' || stage === 'provider_accepted' || stage === 'associated')) throw new Error('输入回执不能将已提交或未知结果标记为可重发。')
  return { message_id: text(data.message_id), target: parseInputTarget(data.target), stage, actual_turn_id: data.actual_turn_id == null ? null : text(data.actual_turn_id), reason: data.reason == null ? null : text(data.reason), retry_safe: retrySafe, control_event: controlEvent }
}
function inputStage(stage: unknown): InputReceipt['stage'] {
  if (stage !== 'entry_accepted' && stage !== 'owner_accepted' && stage !== 'submitting' && stage !== 'provider_accepted' && stage !== 'associated' && stage !== 'refused' && stage !== 'unknown') throw new Error('输入回执无法识别。')
  return stage
}
export function parseScopeContext(value: unknown): ScopeContext {
  const data = record(value), base = { conversation_id: text(data.conversation_id), target: parseInputTarget(data.target) }
  if (data.available === false && data.context == null) return { ...base, available: false, context: null, reason: data.reason == null ? null : text(data.reason) }
  const context = contextProjection(data.context)
  if (data.available !== true || !context || data.reason != null) throw new Error('上下文可读状态与内容不一致。')
  const target = base.target, scope = context.scope
  const matches = target.kind === 'bot_main' ? scope.kind === 'main'
    : target.kind === 'task' || target.kind === 'channel_task' ? scope.kind === 'workspace_task' && scope.task_id === target.task_id
      : target.kind === 'work' ? scope.kind === 'task' && scope.work_id === target.work_id
        : target.kind === 'routine' ? scope.kind === 'routine' && scope.trigger === target.trigger : false
  if (!matches) throw new Error('上下文范围和输入目标不一致。')
  return { ...base, available: true, context, reason: null }
}
export function parsePendingMessage(item: OutboxItem): PendingMessage {
  const data = record(item.params), delivery = item.delivery ?? 'unknown'
  if (!['saved', 'uploading', 'submitting', 'accepted', 'unknown'].includes(delivery)) throw new Error('本地输入状态无法识别。')
  if (item.rejection_code !== undefined && item.rejection_code !== 'input_not_saved' && item.rejection_code !== 'input_conflict') throw new Error('本地输入拒绝原因无法识别。')
  const sequence = number(data.sequence)
  if (!sequence) throw new Error('本地输入顺序无效。')
  return { message_id: text(data.message_id), conversation_id: text(data.conversation_id), text: text(data.text), target: parseInputTarget(data.target), intent: parseInputIntent(data.intent), sequence, expected_revision: optionalNumber(data.expected_revision), attachments: attachments(data.attachments), report_results: data.report_results == null ? undefined : flag(data.report_results), created_at_ms: number(item.created_at_ms), delivery, ...(item.rejected === undefined ? {} : { rejected: item.rejected }), ...(item.rejection_code === undefined ? {} : { rejection_code: item.rejection_code }), ...(item.receipt === undefined ? {} : { receipt: parseInputReceipt(item.receipt) }), ...(item.release_expected_event === undefined ? {} : { release_expected_event: text(item.release_expected_event) }), ...(item.release_command_id === undefined ? {} : { release_command_id: text(item.release_command_id) }) }
}
function taskAction(value: unknown): WorkspaceTask['action'] {
  if (value === 'continue' || value === 'stop' || value === 'cancel' || value === 'defer') return value
  throw new Error('任务操作无法识别。')
}
function taskState(value: unknown): WorkspaceTask['state'] {
  switch (value) {
    case 'not_started': case 'running': case 'waiting_dependencies': case 'waiting_user': case 'update_pending': case 'stop_pending': case 'needs_reconciliation': case 'completed': case 'cancelled': case 'deferred': return value
    default: throw new Error('任务状态无法识别。')
  }
}
function assignmentState(value: unknown): WorkspaceAssignment['state'] {
  switch (value) {
    case 'proposed': case 'accepted': case 'adopted': case 'completed': case 'stopped': case 'unknown': return value
    default: throw new Error('任务分派状态无法识别。')
  }
}
function workspaceChannel(value: unknown): WorkspaceChannel {
  const data = record(value)
  return { channel_id: text(data.channel_id), title: text(data.title), purpose: text(data.purpose), members: texts(data.members), coordinator_bot: text(data.coordinator_bot), coordinator_node: text(data.coordinator_node), owner_epoch: number(data.owner_epoch), revision: number(data.revision), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms) }
}
function workspaceTask(value: unknown): WorkspaceTask {
  const data = record(value)
  return { task_id: text(data.task_id), channel_id: optionalText(data.channel_id), root_task_id: text(data.root_task_id), parent_task_id: optionalText(data.parent_task_id), owner_bot: text(data.owner_bot), title: text(data.title), request: text(data.request), expected_result: text(data.expected_result), source_events: texts(data.source_events), dependencies: texts(data.dependencies), reply_only: flag(data.reply_only), revision: number(data.revision), action: taskAction(data.action), request_event: text(data.request_event), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms), updated_at_ms: number(data.updated_at_ms), state: taskState(data.state), waiting_reason: optionalText(data.waiting_reason), current_assignment: optionalText(data.current_assignment) }
}
function workspaceAssignment(value: unknown): WorkspaceAssignment {
  const data = record(value), submitted = data.submitted_result == null ? null : record(data.submitted_result)
  return { assignment_id: text(data.assignment_id), task_id: text(data.task_id), started_revision: number(data.started_revision), bot_id: text(data.bot_id), execution_node: text(data.execution_node), bot_owner_epoch: number(data.bot_owner_epoch), channel_owner_epoch: optionalNumber(data.channel_owner_epoch), work_id: optionalText(data.work_id), basis: Object.fromEntries(Object.entries(record(data.basis)).map(([id, seq]) => [id, number(seq)])), resource_roots: texts(data.resource_roots), automatic: flag(data.automatic), accepted_revision: optionalNumber(data.accepted_revision), adopted_revision: optionalNumber(data.adopted_revision), completed_revision: optionalNumber(data.completed_revision), trigger_message_id: optionalText(data.trigger_message_id), source_event: optionalText(data.source_event), submitted_result: submitted ? { revision: number(submitted.revision), text: text(submitted.text), event_hash: text(submitted.event_hash) } : undefined, state: assignmentState(data.state), result: optionalText(data.result), event_hash: text(data.event_hash), created_at_ms: number(data.created_at_ms) }
}
export function parseWorkspaceCommand(value: unknown): WorkspaceCommand {
  const data = record(value)
  switch (data.kind) {
    case 'create_channel': return { kind: data.kind, channel_id: text(data.channel_id), title: text(data.title), purpose: text(data.purpose), members: texts(data.members), coordinator_bot: text(data.coordinator_bot), coordinator_node: text(data.coordinator_node) }
    case 'configure_channel': return { kind: data.kind, channel_id: text(data.channel_id), expected_revision: number(data.expected_revision), title: text(data.title), purpose: text(data.purpose), members: texts(data.members), coordinator_bot: text(data.coordinator_bot), coordinator_node: text(data.coordinator_node), owner_epoch: number(data.owner_epoch) }
    case 'post_channel_message': return { kind: data.kind, channel_id: text(data.channel_id), message_id: text(data.message_id), text: text(data.text), attachments: attachments(data.attachments) ?? [], recipient_bot: optionalText(data.recipient_bot), task_id: optionalText(data.task_id), reply_to: optionalText(data.reply_to) }
    case 'create_task': return { kind: data.kind, task_id: text(data.task_id), channel_id: optionalText(data.channel_id), parent_task_id: optionalText(data.parent_task_id), owner_bot: text(data.owner_bot), title: text(data.title), request: text(data.request), expected_result: text(data.expected_result), source_events: texts(data.source_events), dependencies: texts(data.dependencies), reply_only: flag(data.reply_only) }
    case 'revise_task': return { kind: data.kind, task_id: text(data.task_id), expected_revision: number(data.expected_revision), request: text(data.request), expected_result: text(data.expected_result), dependencies: texts(data.dependencies) }
    case 'control_task': return { kind: data.kind, task_id: text(data.task_id), expected_revision: number(data.expected_revision), action: taskAction(data.action) }
    case 'assign_task': return { kind: data.kind, task_id: text(data.task_id), expected_revision: number(data.expected_revision), assignment_id: text(data.assignment_id), bot_id: text(data.bot_id), execution_node: text(data.execution_node), bot_owner_epoch: number(data.bot_owner_epoch), channel_owner_epoch: optionalNumber(data.channel_owner_epoch), work_id: optionalText(data.work_id), resource_roots: texts(data.resource_roots), automatic: flag(data.automatic) }
    case 'assignment_progress': return { kind: data.kind, assignment_id: text(data.assignment_id), revision: number(data.revision), state: assignmentState(data.state), input_event: optionalText(data.input_event), evidence_event: optionalText(data.evidence_event), ...(data.execution == null ? {} : { execution: turnExecution(data.execution) }), result: optionalText(data.result) }
    case 'control_input': return { kind: data.kind, message_id: text(data.message_id), expected_event: text(data.expected_event), action: inputControlAction(data.action) }
    default: throw new Error('操作类型无法识别。')
  }
}
function workspaceProjection(value: unknown) {
  const data = record(value)
  return { channels: list(data.channels).map(workspaceChannel), tasks: list(data.tasks).map(workspaceTask), assignments: list(data.assignments).map(workspaceAssignment), inputs: list(data.inputs).map(queuedInput) }
}
export function parseWorkspaceState(value: unknown): WorkspaceDomainState {
  const data = record(value), coverage = record(data.coverage)
  return { ...workspaceProjection(data), coverage: { limit: number(coverage.limit), truncated: flag(coverage.truncated) } }
}
export function parseWorkspaceCommandResult(value: unknown): WorkspaceCommandResult {
  const data = record(value)
  return { ...workspaceProjection(data), command_id: text(data.command_id), event_hash: text(data.event_hash), duplicate: flag(data.duplicate) }
}
export function parseWorkspaceCommandNotSaved(value: unknown, expectedCommandId: unknown): WorkspaceCommandNotSaved | null {
  const data = record(value)
  if (data.code !== 'workspace_command_not_saved') return null
  const commandId = text(data.command_id)
  if (data.accepted !== false || data.stored !== false || commandId !== expectedCommandId) throw new Error('操作拒绝回执和原操作不一致，内容已保留。请再查一次。')
  return { accepted: false, code: data.code, stored: false, command_id: commandId, reason: text(data.reason) }
}
function workspaceHistoryBody(value: unknown): WorkspaceHistoryBody {
  const data = record(value)
  switch (data.type) {
    case 'input_posted': return { type: data.type, ...workspaceInputFields(data) }
    case 'workspace_commanded': return { type: data.type, command_id: text(data.command_id), command: parseWorkspaceCommand(data.command) }
    case 'message_posted': return { type: data.type, message_id: text(data.message_id), role: text(data.role), text: text(data.text), attachments: attachments(data.attachments), in_reply_to: optionalText(data.in_reply_to), status: data.status == null ? undefined : replyStatus(data.status), ...failure(data.failure) }
    case 'work_dispatched': return { type: data.type, work_id: text(data.work_id), title: text(data.title), prompt: text(data.prompt) }
    case 'work_progress': return { type: data.type, work_id: text(data.work_id), phase: workPhase(data.phase), summary: text(data.summary), thread_id: optionalText(data.thread_id), turn_id: optionalText(data.turn_id) }
    case 'work_finished': {
      const outcome = workOutcome(data.outcome)
      if (!outcome) throw new Error('工作结果缺少终态。')
      return { type: data.type, work_id: text(data.work_id), outcome, result_text: text(data.result_text), failure_reason: optionalText(data.failure_reason), ...failure(data.failure) }
    }
    default: throw new Error('历史记录的类型无法识别。')
  }
}
export function parseWorkspaceHistory(value: unknown): WorkspaceHistory {
  const data = record(value), coverage = record(data.coverage), scope = coverage.scope
  if (scope !== 'task' && scope !== 'channel' && scope !== 'workspace') throw new Error('历史记录的范围无法识别。')
  return { events: list(data.events).map((value) => {
    const item = record(value), body = workspaceHistoryBody(item.body)
    if (item.kind !== body.type) throw new Error('历史记录的类型与内容不一致。')
    return { event_hash: text(item.event_hash), created_at_ms: number(item.created_at_ms), scope_id: text(item.scope_id), kind: body.type, body, speaker: workspaceSpeaker(item.speaker) }
  }), coverage: { limit: number(coverage.limit), truncated: flag(coverage.truncated), scope } }
}
function workspaceSpeaker(value: unknown): WorkspaceSpeaker {
  const data = record(value)
  switch (data.kind) {
    case 'human': case 'node': return { kind: data.kind, device_id: text(data.device_id) }
    case 'bot': return { kind: data.kind, bot_id: text(data.bot_id) }
    default: throw new Error('无法识别发言者。')
  }
}
function recordOrNull(value: unknown) { return value == null ? null : record(value) }
function providerStage(value: unknown): ProviderStage {
  const stage = providerStages.find((stage) => stage === value)
  if (!stage) throw new Error('回合阶段无法识别。')
  return stage
}
export function parseTurnRuntime(value: unknown): TurnRuntime | null {
  if (value == null) return null
  const data = record(value), previous = recordOrNull(data.switched_from)
  const nullable = (value: unknown) => value == null ? null : text(value)
  return { provider: provider(data.provider), model: nullable(data.model), reasoning_effort: nullable(data.reasoning_effort), generation: text(data.generation), session_generation: number(data.session_generation), delivery_id: text(data.delivery_id), stage: providerStage(data.stage), execution: turnExecution(data.execution), attempted_at_ms: number(data.attempted_at_ms), switched_from: previous ? { provider: provider(previous.provider), model: nullable(previous.model), reasoning_effort: nullable(previous.reasoning_effort) } : null, switch_reason: failureKind(data.switch_reason) ?? null }
}
function turnExecution(value: unknown): TurnRuntime['execution'] {
  if (value === 'not_started' || value === 'started' || value === 'unknown') return value
  throw new Error('执行状态无法识别。')
}
function agentLane(value: unknown): AgentLane | undefined {
  switch (value) {
    case null: case undefined: return undefined
    case 'main': case 'task': case 'schedule_routine': case 'bot_message': return value
    default: throw new Error('未知的任务通道。')
  }
}
function queueHold(value: unknown): QueueHoldReason | undefined {
  switch (value) {
    case null: case undefined: return undefined
    case 'active_turn': case 'steering_settlement': case 'provider_serial': case 'task_limit': case 'machine_slot': case 'pending_check': case 'backend_config_pending': case 'attachment_preparation': case 'earlier_input': case 'resource_conflict': return value
    default: throw new Error('未知的排队原因。')
  }
}
function contextProjection(value: unknown): ContextProjection | null {
  if (value == null) return null
  const data = record(value), coverage = record(data.coverage)
  const stage = data.stage, provider = data.provider
  if (stage !== 'compiled' && stage !== 'started' && stage !== 'completed' && stage !== 'failed' && stage !== 'cancelled' && stage !== 'interrupted' && stage !== 'unknown') throw new Error('未知的上下文状态。')
  if (provider !== 'codex' && provider !== 'cursor' && provider !== 'claude' && provider !== 'pi' && provider !== 'opencode') throw new Error('未知的 AI 服务。')
  const fingerprint = (value: unknown) => { const digest = text(value); if (!/^[a-f0-9]{64}$/.test(digest)) throw new Error('上下文指纹格式不对。'); return digest }
  const session = data.session_status == null ? null : record(data.session_status)
  return {
    scope: parseSessionScope(data.scope),
    sources: list(data.sources).map((value) => {
      const item = record(value), kind = item.kind
      if (kind !== 'trigger' && kind !== 'message' && kind !== 'work' && kind !== 'dispatch' && kind !== 'checkpoint' && kind !== 'handoff' && kind !== 'memory' && kind !== 'question' && kind !== 'attachment') throw new Error('上下文来源类型无法识别。')
      return { kind, event_id: optionalText(item.event_id), author: optionalText(item.author), seq: optionalNumber(item.seq), reference: optionalText(item.reference) }
    }),
    sources_omitted: number(data.sources_omitted),
    inputs: list(data.inputs).map((value) => {
      const item = record(value)
      return { message_id: text(item.message_id), source_event: text(item.source_event), stage: inputStage(item.stage), actual_turn_id: item.actual_turn_id == null ? null : text(item.actual_turn_id), expected_turn_id: text(item.expected_turn_id), accepted_at_ms: optionalNumber(item.accepted_at_ms) }
    }),
    inputs_omitted: number(data.inputs_omitted),
    attempt_id: text(data.attempt_id), provider, stage,
    stable_fingerprint: fingerprint(data.stable_fingerprint), dynamic_fingerprint: fingerprint(data.dynamic_fingerprint),
    session_generation: number(data.session_generation), stable_bytes: number(data.stable_bytes), dynamic_bytes: number(data.dynamic_bytes),
    invalidation_reason: data.invalidation_reason == null ? null : text(data.invalidation_reason),
    ...(data.session_status === undefined ? {} : { session_status: session && { resumable: flag(session.resumable), pending_generation: flag(session.pending_generation) } }),
    coverage: {
      messages_omitted: number(coverage.messages_omitted), works_omitted: number(coverage.works_omitted), dispatches_omitted: number(coverage.dispatches_omitted),
      questions_omitted: number(coverage.questions_omitted), attachments_omitted: number(coverage.attachments_omitted), memory_omitted: number(coverage.memory_omitted),
      checkpoint_omitted: number(coverage.checkpoint_omitted), handoff_omitted: number(coverage.handoff_omitted), pending_checks_omitted: number(coverage.pending_checks_omitted),
      bot_indexes_omitted: Object.fromEntries(Object.entries(record(coverage.bot_indexes_omitted)).map(([kind, count]) => [kind, number(count)])),
      history_retrieval: text(coverage.history_retrieval),
    },
  }
}
function memoryConflicts(value: unknown): MemoryConflict[] | null {
  if (value == null) return null
  return list(value).map((value) => {
    const data = record(value)
    return { call_ref: text(data.call_ref), memory_id: text(data.memory_id), expected_epoch: number(data.expected_epoch), actual_epoch: number(data.actual_epoch), expected_revision: text(data.expected_revision), actual_revision: text(data.actual_revision), reason: text(data.reason) }
  })
}
function schedulerSnapshot(value: unknown): SchedulerSnapshot | null {
  if (value == null) return null
  const data = record(value), capabilities = data.capabilities == null ? null : record(data.capabilities), remote = data.remote == null ? null : record(data.remote)
  return {
    unanswered: list(data.unanswered).map(text),
    jobs: list(data.jobs).map(managerJob),
    scope_jobs: list(data.scope_jobs).map(scopeJob),
    ...(data.context === undefined ? {} : { context: contextProjection(data.context) }),
    provider: optionalText(data.provider), model: optionalText(data.model), capabilities: capabilities ? { streaming: flag(capabilities.streaming), cancel: flag(capabilities.cancel), steer: flag(capabilities.steer) } : undefined,
    remote: remote ? { owner: text(remote.owner), reachable: flag(remote.reachable) } : undefined,
  }
}
function scopeJob(value: unknown): ScopeJob {
  const data = record(value), kind = data.kind, lane = agentLane(data.lane)
  if (kind !== 'human' && kind !== 'answer' && kind !== 'work_result' && kind !== 'agent_task' && kind !== 'workspace_task' && kind !== 'schedule' && kind !== 'bot_message') throw new Error('目标任务的触发类型无法识别。')
  if (!lane) throw new Error('目标任务缺少任务通道。')
  return { conversation_id: text(data.conversation_id), trigger: text(data.trigger), kind, scope: parseSessionScope(data.scope), state: text(data.state), sequence: number(data.sequence), lane, source_node: text(data.source_node), owner: data.owner === null ? null : text(data.owner), message_id: data.message_id === null ? null : text(data.message_id), reply_message_id: text(data.reply_message_id), ...(data.hold_reason == null ? {} : { hold_reason: queueHold(data.hold_reason) }) }
}
function sentCoverage(value: unknown): SentCoverage {
  const data = record(value)
  if (data.scope !== 'main') throw new Error('Bot 发出消息的来源范围无法确认。')
  return { scope: data.scope, omitted_unknown: number(data.omitted_unknown) }
}
function managerJob(value: unknown): ManagerJob {
  const job = record(value)
  const into = job.into == null ? null : record(job.into), receipt = job.receipt == null ? null : record(job.receipt)
  return { trigger: text(job.trigger), state: text(job.state), scope: parseSessionScope(job.scope), lane: agentLane(job.lane), hold_reason: queueHold(job.hold_reason),
    ...(into ? { into: { provider: provider(into.provider), attempt_id: text(into.attempt_id), thread_id: text(into.thread_id), turn_id: text(into.turn_id) } } : {}),
    ...(receipt ? { receipt: { accepted_at_ms: number(receipt.accepted_at_ms), actual_turn_id: receipt.actual_turn_id == null ? null : text(receipt.actual_turn_id) } } : {}),
    ...(job.uncertain === undefined ? {} : { uncertain: flag(job.uncertain) }),
    message_id: optionalText(job.message_id), reply_message_id: optionalText(job.reply_message_id),
    failure: job.failure == null ? undefined : { kind: failureKind(record(job.failure).kind) ?? 'unknown', detail: text(record(job.failure).detail) },
    runtime: parseTurnRuntime(job.runtime), retryable: job.retryable === true && recordOrNull(job.runtime)?.execution === 'not_started' && job.pending_check !== true,
    pending_check: job.pending_check === true, outcome: optionalText(job.outcome) as SteerOutcome | undefined, source_node: optionalText(job.source_node),
    ...(job.context === undefined ? {} : { context: contextProjection(job.context) }),
  }
}
export function parseSessionScope(value: unknown): SessionScope {
  const data = record(value)
  switch (data.kind) {
    case 'main': return { kind: data.kind }
    case 'workspace_task': return { kind: data.kind, task_id: text(data.task_id) }
    case 'task': return { kind: data.kind, work_id: text(data.work_id) }
    case 'routine': return { kind: data.kind, trigger: text(data.trigger) }
    default: throw new Error('会话范围无法识别。')
  }
}
function workState(value: unknown): WorkState | DispatchState {
  switch (value) {
    case "accepted": case "running": case "interrupting": case "finished": case "unverified":
    case "dispatch_waiting": case "dispatch_rejected": case "dispatch_unverified": return value
    default: throw new Error("任务状态无法识别。")
  }
}
function workOutcome(value: unknown): WorkOutcome | null {
  switch (value) {
    case undefined: case null: return null
    case "completed": return "completed"
    case "failed": return "failed"
    case "interrupted": return "interrupted"
    case "rejected": return "rejected"
    case "unverified": return "unverified"
    default: throw new Error("任务结果无法识别。")
  }
}
function workPhase(value: unknown): WorkPhase {
  switch (value) {
    case "started": case "running": case "interrupting": case "finishing": return value
    default: throw new Error("任务阶段无法识别。")
  }
}
function optionalPhase(value: unknown): WorkPhase | undefined { return value == null ? undefined : workPhase(value) }
function workKind(value: unknown): WorkRecord["kind"] {
  if (value == null) return undefined
  if (value === "task" || providerIds.includes(value as ProviderId)) return value as WorkRecord["kind"]
  throw new Error("任务类型无法识别。")
}
function optionalWorkFields(data: Record<string, unknown>): Partial<Omit<WorkRecord, "state" | "resolved">> {
  const fields: Partial<Omit<WorkRecord, "state" | "resolved">> = {}
  if (data.target != null) fields.target = parseInputTarget(data.target)
  for (const key of ["dispatcher", "call_ref", "thread_id", "turn_id"] as const) {
    if (data[key] != null) fields[key] = text(data[key])
  }
  const kind = workKind(data.kind)
  if (kind !== undefined) fields.kind = kind
  if (typeof data.model === "string") fields.model = data.model
  if (data.coordination_job !== undefined) fields.coordination_job = data.coordination_job === null ? null : scopeJob(data.coordination_job)
  return fields
}
function work(value: unknown): WorkRecord {
  const data = record(value)
  const common = {
    work_id: text(data.work_id), trigger: text(data.trigger), target_node: text(data.target_node), cwd: text(data.cwd), title: data.title == null ? "" : text(data.title), prompt: text(data.prompt),
    summary: text(data.summary), outcome: workOutcome(data.outcome), result_text: text(data.result_text),
    failure_reason: data.failure_reason == null ? null : text(data.failure_reason), ...failure(data.failure), ...optionalWorkFields(data),
  }
  const state = workState(data.state)
  if (state === "dispatch_waiting" || state === "dispatch_rejected" || state === "dispatch_unverified") return { ...common, state }
  const resolved = record(data.resolved)
  return { ...common, state, resolved: { provider: provider(resolved.provider), model: resolved.model === null ? null : text(resolved.model) },
    dispatcher: text(data.dispatcher), call_ref: text(data.call_ref),
    continues: data.continues === null ? null : text(data.continues), parent: data.parent === null ? null : text(data.parent),
    thread: data.thread === null ? null : text(data.thread), phase: data.phase === null ? null : workPhase(data.phase) }
}
/** Requests this device still carries: an untaken dispatch shows as its own task row, a stop marks its work. */
export function withDeliveries(works: WorkRecord[], value: unknown): WorkRecord[] {
  const rows = [...works]
  for (const item of list(value)) {
    const data = record(item), state = data.state, work_id = text(data.work_id), target_node = text(data.target_node)
    if (state === "dispatch_waiting" || state === "dispatch_rejected" || state === "dispatch_unverified") {
      rows.push({ work_id, trigger: data.trigger == null ? "" : text(data.trigger), target_node, cwd: text(data.cwd), title: data.title == null ? "" : text(data.title), prompt: data.prompt == null ? "" : text(data.prompt), state, summary: "", outcome: null, result_text: "", failure_reason: data.reason == null ? null : text(data.reason), ...failure(data.failure) })
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
export function confirmReason(item: Pick<WorkCard, "state" | "outcome" | "target_node" | "kind">, nodes: readonly NodeInfo[]): ConfirmReason | null {
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
export function workChecks(works: readonly OpenWorkCard[], nodes: readonly NodeInfo[]): PendingCheck[] {
  const checks = works.flatMap((item): WorkCheck[] => {
    const reason = confirmReason(item, nodes)
    return reason ? [{ kind: "work", check_id: `work:${item.work_id}`, detail: item.failure_reason ?? "", work_id: item.work_id, ...(item.target === undefined ? {} : { target: item.target }), target_node: item.target_node, conversation_id: item.conversation_id, title: item.title, reason }] : []
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
const openWork = (item: unknown): OpenWork => ({ ...work(item), conversation_id: text(record(item).conversation_id) })
export function parseOpenWorks(value: unknown): OpenWork[] {
  return list(record(value).works).map(openWork)
}
export function parseWorkDetail(value: unknown): WorkDetail {
  const data = record(value), coverage = record(data.coverage), shared = record(data.work), sourceEvents = texts(shared.source_events)
  if (coverage.source !== 'shared_history' || coverage.complete !== false) throw new Error('工作详情的历史范围无法确认。')
  if (sourceEvents.length > 64) throw new Error('工作详情的来源引用超出范围。')
  return { work: { ...openWork(shared), target: shared.target === null ? null : parseInputTarget(shared.target), source_events: sourceEvents }, coverage: { source: 'shared_history', complete: false } }
}

export type WorkspaceOverviewInput = {
  openWorks: readonly OpenWorkCard[]
  bots: readonly BotRecord[]
  questions: readonly Question[]
  nodes: readonly NodeInfo[]
}

const overviewBot = (bot: Pick<BotRecord, "bot_id" | "conversation_id" | "name">): OverviewBot => ({ bot_id: bot.bot_id, conversation_id: bot.conversation_id, name: bot.name })
const overviewNode = (node: NodeInfo): OverviewNode => ({ device_id: node.device_id, name: node.name, online: node.online, revoked: node.revoked, local: node.local })
const nullableText = (value: unknown): string | null => value == null ? null : text(value)
const nullableNumber = (value: unknown): number | null => value == null ? null : number(value)

function overviewConsistency(value: unknown): OverviewConsistency {
  return value === "local_snapshot" || value === "mixed_remote" || value === "partial" ? value : "partial"
}
function overviewState(value: unknown): OverviewTaskState {
  switch (value) {
    case "accepted": case "running": case "interrupting": case "finished": case "unverified":
    case "dispatch_waiting": case "dispatch_rejected": case "dispatch_unverified": case "stop_waiting": case "stop_timed_out": case "accepted_waiting_sync": case "unknown": return value
    default: return "unknown"
  }
}
function overviewOutcome(value: unknown): WorkOutcome | null {
  if (value == null) return null
  switch (value) {
    case "completed": return "completed"
    case "failed": return "failed"
    case "interrupted": return "interrupted"
    case "rejected": return "rejected"
    case "unverified": return "unverified"
    default: return "unverified"
  }
}
function overviewSource(value: unknown): OverviewTask["source"] {
  return value === "local_dispatch" || value === "local_check" || value === "remote_owner" || value === "event" || value === "dag" ? value : "event"
}
function overviewDispatchState(value: unknown): OverviewDeliveryState {
  switch (value) {
    case "dispatch_waiting": case "dispatch_rejected": case "dispatch_unverified": return value
    case "dispatch_accepted": case "stop_waiting": case "stop_timed_out": return value
    case "accepted_waiting_sync": return "dispatch_accepted"
    default: return "dispatch_unverified"
  }
}
function overviewWorkKind(value: unknown): ProviderId | "task" | null {
  if (value === "task") return value
  return providerIds.find((provider) => provider === value) ?? null
}
function overviewNodeFromValue(value: unknown): OverviewNode | null {
  if (value == null) return null
  const data = record(value)
  return {
    device_id: text(data.device_id), name: data.name == null ? "" : text(data.name),
    online: data.online === true, revoked: data.revoked === true, local: data.local === true,
  }
}

function safeOverviewOutcome(outcome: WorkOutcome | null, state: OverviewTaskState): WorkOutcome | null {
  return outcome === "completed" && (state === "unknown" || state === "unverified" || state === "dispatch_unverified" || state === "accepted_waiting_sync") ? "unverified" : outcome
}

function overviewTaskBase(work: Pick<OpenWork, "work_id" | "conversation_id" | "title" | "summary" | "cwd" | "state" | "outcome" | "target_node" | "dispatcher" | "call_ref" | "parent" | "continues" | "thread" | "thread_id" | "turn_id" | "phase" | "failure_reason" | "failure" | "kind" | "stop" | "coordination_job">, bot: OverviewBot | null, node: OverviewNode | null, source: OverviewTask["source"], itemId = work.work_id) {
  const navigation = bot ? { bot_id: bot.bot_id, conversation_id: bot.conversation_id, work_id: work.work_id } : null
  const rawState = overviewState(work.stop ?? work.state)
  const outcome = safeOverviewOutcome(work.outcome, rawState)
  return {
    item_id: itemId,
    work_id: work.work_id,
    conversation_id: work.conversation_id,
    bot,
    title: work.title,
    summary: work.summary,
    cwd: work.cwd,
    state: outcome === "unverified" ? "unverified" : work.state === "finished" && work.outcome === null ? "unknown" : rawState,
    outcome,
    target_node: work.target_node,
    dispatcher: work.dispatcher ?? null,
    call_ref: work.call_ref ?? null,
    work_kind: work.kind ?? null,
    parent: work.parent ?? null,
    continues: work.continues ?? null,
    thread: work.thread ?? null,
    thread_id: work.thread_id ?? null,
    turn_id: work.turn_id ?? null,
    phase: work.phase ?? null,
    ...(work.coordination_job === undefined ? {} : { coordination_job: work.coordination_job }),
    reason: work.failure_reason,
    ...failure(work.failure),
    node,
    navigation,
    source,
  }
}

function openOverviewWork(work: OpenWorkCard, bots: ReadonlyMap<string, OverviewBot>, nodes: ReadonlyMap<string, OverviewNode>): OverviewTask | null {
  const bot = bots.get(work.conversation_id) ?? null, node = nodes.get(work.target_node) ?? null
  const unsafeCompleted = work.outcome === "completed" && work.state === "unverified"
  // A failure stays listed until a later work of its conversation ends, as the Node lists it.
  const failed = work.outcome === "failed" || work.outcome === "rejected"
  if (work.outcome && work.outcome !== "unverified" && !unsafeCompleted && !failed) return null
  if (work.state === "finished" && work.outcome !== "unverified" && !unsafeCompleted && !failed) return null
  if (work.state === "dispatch_waiting" || work.state === "dispatch_rejected" || work.state === "dispatch_unverified") {
    return { ...overviewTaskBase(work, bot, node, "local_dispatch"), kind: "dispatch", delivery_state: work.state }
  }
  return { ...overviewTaskBase(work, bot, node, "event"), kind: "work" }
}

function overviewCheck(check: PendingCheck, workById: ReadonlyMap<string, OpenWorkCard>, bots: ReadonlyMap<string, OverviewBot>, nodes: ReadonlyMap<string, OverviewNode>): OverviewTask {
  const works = check.kind === "device" ? check.works.flatMap((item) => { const work = workById.get(item.work_id); return work ? [work] : [] }) : check.kind === "work" ? [workById.get(check.work_id)].filter((work): work is OpenWorkCard => Boolean(work)) : []
  const first = works[0]
  const fallback: OpenWorkCard = first ?? {
    work_id: check.kind === "work" ? check.work_id : "",
    conversation_id: check.kind === "work" ? check.conversation_id : "",
    title: check.kind === "work" ? check.title : "待确认",
    summary: "",
    state: "unverified",
    outcome: "unverified",
    target_node: check.kind === "work" || check.kind === "device" ? check.target_node : "",
    cwd: "", failure_reason: check.detail,
  }
  const bot = fallback.conversation_id ? bots.get(fallback.conversation_id) ?? null : null
  const node = fallback.target_node ? nodes.get(fallback.target_node) ?? null : null
  const base = overviewTaskBase(fallback, bot, node, "local_check", check.check_id)
  const checkReason: ConfirmReason = check.kind === "process" ? "unverified" : check.reason
  return {
    ...base,
    kind: "check",
    check_id: check.check_id,
    work_ids: works.map((work) => work.work_id),
    state: "unknown",
    outcome: checkReason === "unverified" ? "unverified" : null,
    reason: check.detail || checkReason,
    navigation: check.kind === "device" || !bot ? (bot ? { bot_id: bot.bot_id, conversation_id: bot.conversation_id } : null) : base.navigation,
  }
}

/**
 * Derive the finite, shared workspace view available from the already loaded lists.
 * `open_works` is intentionally incomplete, so coverage says so instead of implying a global queue.
 */
export function overviewFromWorkspace({ openWorks, bots, questions, nodes }: WorkspaceOverviewInput): WorkspaceOverview {
  const botRows = bots.map(overviewBot)
  const botByConversation = new Map(botRows.map((bot) => [bot.conversation_id, bot]))
  const nodeById = new Map(nodes.map((node) => [node.device_id, overviewNode(node)]))
  const workById = new Map(openWorks.map((work) => [work.work_id, work]))
  const tasks = openWorks.flatMap((work) => {
    const row = openOverviewWork(work, botByConversation, nodeById)
    return row ? [row] : []
  })
  const checks = workChecks(openWorks, nodes).map((check) => overviewCheck(check, workById, botByConversation, nodeById))
  const overviewQuestions = questions.filter(questionOpen).map((question): OverviewQuestion => {
    const bot = botByConversation.get(question.conversation_id) ?? null
    const related = question.about.request_id ? openWorks.find((work) => work.thread_id === question.about.session_id && !work.outcome) : undefined
    return {
      kind: "question", question_id: question.question_id, conversation_id: question.conversation_id, bot,
      attention: question.attention, text: question.text, severity: question.severity, choices: question.choices,
      request_id: question.about.request_id ?? null, work_id: related?.work_id ?? null, about: question.about, created_at_ms: question.created_at_ms,
      state: "unanswered", navigation: bot ? { bot_id: bot.bot_id, conversation_id: bot.conversation_id } : null,
    }
  })
  const remoteOwners: OverviewCoverage["remote_owners"] = nodes.filter((node) => !node.local).map((node) => ({
    owner: node.device_id, reachable: node.online && !node.revoked, reason: node.revoked ? "revoked" : node.online ? null : "unreachable",
  }))
  const unreachable = nodes.filter((node) => !node.local && (!node.online || node.revoked)).map((node) => node.device_id)
  const consistency: OverviewConsistency = unreachable.length ? "partial" : "local_snapshot"
  const coverage: OverviewCoverage = {
    source: "open_works", complete: false, bounded: true, generated_at_ms: null, network_epoch: null, consistency,
    truncated: false, local_heads: false, remote_owners: remoteOwners, unreachable_nodes: unreachable,
    omitted_finished: null, omitted_sessions: null, omitted_tasks: null, omitted_questions: null, omitted_checks: null, omitted_dispatches: null, omitted_deliveries: null, omitted_relations: null, omitted_jobs: null, omitted_live_progress: true,
  }
  return {
    schema: null, generated_at_ms: null, network_epoch: null, heads: {}, consistency, bots: botRows, conversations: [],
    tasks: [...tasks, ...checks], questions: overviewQuestions, jobs: [], relations: [], sessions: [], coverage, truncated: false, next_cursor: null,
  }
}

function parseOverviewBot(value: unknown): OverviewBot {
  const data = record(value)
  return { bot_id: text(data.bot_id), conversation_id: text(data.conversation_id), name: data.name == null ? "" : text(data.name) }
}
function parseOverviewTask(value: unknown): OverviewTask {
  const data = record(value)
  const rawKind = data.item_kind ?? data.kind
  const rawState = data.state
  const itemKind = rawKind === "check" ? "check" : rawKind === "dispatch" || data.source === "local_dispatch" || rawState === "accepted_waiting_sync" || rawState === "dispatch_waiting" || rawState === "dispatch_rejected" || rawState === "dispatch_unverified" || rawState === "dispatch_accepted" || rawState === "stop_waiting" || rawState === "stop_timed_out" ? "dispatch" : "work"
  const workId = nullableText(data.work_id)
  const conversationId = text(data.conversation_id)
  const bot = data.bot == null ? null : parseOverviewBot(data.bot)
  const state = overviewState(data.state)
  const node = overviewNodeFromValue(data.node) ?? (data.remote_reachable === false ? { device_id: nullableText(data.target_node) ?? "", name: "", online: false, revoked: false, local: false } : null)
  const outcome = safeOverviewOutcome(overviewOutcome(data.outcome), state)
  const common = {
    item_id: nullableText(data.item_id) ?? nullableText(data.check_id) ?? workId ?? `${conversationId}:${text(data.trigger ?? "")}`,
    work_id: workId, conversation_id: conversationId, bot, title: data.title == null ? "" : text(data.title), summary: data.summary == null ? "" : text(data.summary), cwd: data.cwd == null ? "" : text(data.cwd),
    state: outcome === "unverified" ? "unverified" : state === "finished" && outcome === null ? "unknown" : state, outcome,
    target_node: nullableText(data.target_node), dispatcher: nullableText(data.dispatcher), call_ref: nullableText(data.call_ref),
    work_kind: overviewWorkKind(data.work_kind) ?? overviewWorkKind(data.kind),
    parent: nullableText(data.parent), continues: nullableText(data.continues), thread: nullableText(data.thread), thread_id: nullableText(data.thread_id), turn_id: nullableText(data.turn_id),
    phase: data.phase == null ? null : optionalPhase(data.phase) ?? null, reason: nullableText(data.reason ?? data.failure_reason), ...failure(data.failure), node, source: overviewSource(data.source),
    navigation: bot ? { bot_id: bot.bot_id, conversation_id: bot.conversation_id, ...(workId ? { work_id: workId } : {}) } : null,
    ...(data.coordination_job === undefined ? {} : { coordination_job: data.coordination_job === null ? null : scopeJob(data.coordination_job) }),
  }
  if (itemKind === "dispatch") return { ...common, kind: "dispatch", delivery_state: overviewDispatchState(data.delivery_state ?? data.state) }
  if (itemKind === "check") return { ...common, kind: "check", check_id: text(data.check_id ?? common.item_id), work_ids: list(data.work_ids ?? (workId ? [workId] : [])).map(text), reason: data.reason === "offline" || data.reason === "disconnected" || data.reason === "unverified" ? data.reason : "unverified", state: "unknown", outcome: data.reason === "unverified" ? "unverified" : null }
  return { ...common, kind: "work" }
}
function parseOverviewQuestion(value: unknown): OverviewQuestion {
  const data = record(value), bot = data.bot == null ? null : parseOverviewBot(data.bot)
  const about = data.about == null ? null : subject(data.about)
  return {
    kind: "question", question_id: text(data.question_id), conversation_id: text(data.conversation_id), bot,
    attention: data.attention == null ? undefined : parseAttention(data.attention), text: text(data.text), severity: severity(data.severity), choices: list(data.choices ?? []).map(questionChoice),
    request_id: nullableText(data.request_id ?? about?.request_id), work_id: nullableText(data.work_id), about, created_at_ms: number(data.created_at_ms), state: "unanswered",
    navigation: bot ? { bot_id: bot.bot_id, conversation_id: bot.conversation_id } : null,
  }
}
function parseOverviewJob(value: unknown): OverviewJob {
  const data = record(value), conversationId = nullableText(data.conversation_id), trigger = data.trigger == null ? "" : text(data.trigger), messageId = nullableText(data.message_id), unknownReason = nullableText(data.unknown_reason)
  const state = data.remote_reachable === false || unknownReason ? "unknown" : data.state == null ? "unknown" : text(data.state)
  return {
    job_id: nullableText(data.job_id) ?? `${conversationId ?? ""}:${trigger}:${messageId ?? ""}`, conversation_id: conversationId, trigger, state,
    runtime: parseTurnRuntime(data.runtime), retryable: data.retryable === true && recordOrNull(data.runtime)?.execution === "not_started" && data.pending_check !== true && state === "failed",
    message_id: messageId, reply_message_id: nullableText(data.reply_message_id), source_node: nullableText(data.source_node), pending_check: data.pending_check === true,
    outcome: state === "unknown" ? "unknown" : data.outcome === "answered" || data.outcome === "unfinished" || data.outcome === "unknown" ? data.outcome : null,
    unknown_reason: unknownReason, kind: nullableText(data.kind), sequence: nullableNumber(data.sequence), owner_epoch: nullableNumber(data.owner_epoch),
    ...(data.lane === undefined ? {} : { lane: agentLane(data.lane) }),
    ...(data.hold_reason === undefined ? {} : { hold_reason: queueHold(data.hold_reason) }),
    ...(data.context === undefined ? {} : { context: contextProjection(data.context) }),
  }
}
function parseOverviewConversation(value: unknown): OverviewConversation {
  const data = record(value)
  return { conversation_id: text(data.conversation_id), title: data.title == null ? "" : text(data.title), owner: nullableText(data.owner), owner_epoch: number(data.owner_epoch) }
}
function parseOverviewSession(value: unknown): OverviewSession {
  const data = record(value)
  return { conversation_id: text(data.conversation_id), node: text(data.node), session_id: text(data.session_id), agent: provider(data.agent), muted: data.muted === true, created_at_ms: number(data.created_at_ms), version: text(data.version), cwd: data.cwd == null ? "" : text(data.cwd), summary: data.summary == null ? "" : text(data.summary) }
}
function parseOverviewCheck(value: unknown): OverviewTask {
  const data = record(value), checkId = text(data.check_id), conversationId = text(data.conversation_id), reason = nullableText(data.unknown_reason) ?? "process_unconfirmed"
  const bot = data.bot == null ? null : parseOverviewBot(data.bot)
  const trigger = data.trigger == null ? "" : text(data.trigger)
  const workId = trigger.startsWith("task:") ? trigger.slice(5) || null : null
  return {
    kind: "check", item_id: checkId, check_id: checkId, work_ids: workId ? [workId] : [], work_id: workId, conversation_id: conversationId, bot,
    title: "待确认", summary: trigger, cwd: "", state: "unknown", outcome: "unverified",
    target_node: null, dispatcher: null, call_ref: null, work_kind: null, parent: null, continues: null, thread: null, thread_id: null, turn_id: null, phase: null,
    reason, node: null, navigation: bot ? { bot_id: bot.bot_id, conversation_id: conversationId } : null, source: "local_check",
  }
}
function parseOverviewRelation(value: unknown): OverviewRelation {
  const data = record(value), kind = data.kind
  if (kind !== "parent" && kind !== "continues" && kind !== "dispatches") throw new Error("任务关系无法识别。")
  return { from_work: text(data.from_work), to_work: text(data.to_work), kind }
}
function parseOverviewCoverage(value: unknown, consistency: OverviewConsistency, truncated: boolean, generatedAt: number | null, networkEpoch: number | null): OverviewCoverage {
  const data = value == null ? {} : record(value)
  const remote = list(data.remote_owners ?? []).map((item) => {
    const owner = record(item)
    return { owner: nullableText(owner.owner), reachable: owner.reachable == null ? null : owner.reachable === true, reason: nullableText(owner.reason), ...(owner.status == null ? {} : { status: text(owner.status) }) }
  })
  const unreachable = list(data.unreachable_nodes ?? []).map(text)
  return {
    source: data.source === "workspace_overview" || data.local_heads === true ? "workspace_overview" : "open_works", complete: data.complete === true, bounded: data.bounded !== false,
    generated_at_ms: nullableNumber(data.generated_at_ms ?? generatedAt), network_epoch: nullableNumber(data.network_epoch ?? networkEpoch), consistency: overviewConsistency(data.consistency ?? consistency), truncated: data.truncated === true || truncated,
    local_heads: data.local_heads === true, remote_owners: remote, unreachable_nodes: unreachable,
    omitted_finished: nullableNumber(data.omitted_finished), omitted_sessions: nullableNumber(data.omitted_sessions), omitted_tasks: nullableNumber(data.omitted_tasks), omitted_questions: nullableNumber(data.omitted_questions), omitted_checks: nullableNumber(data.omitted_checks), omitted_dispatches: nullableNumber(data.omitted_dispatches), omitted_deliveries: nullableNumber(data.omitted_deliveries), omitted_relations: nullableNumber(data.omitted_relations), omitted_jobs: nullableNumber(data.omitted_jobs), omitted_live_progress: data.omitted_live_progress !== false,
  }
}

/** Parse the bounded `workspace_overview` response without claiming unknown state is complete. */
export function parseWorkspaceOverview(value: unknown): WorkspaceOverview {
  const data = record(value), consistency = overviewConsistency(data.consistency), generatedAt = nullableNumber(data.generated_at_ms), networkEpoch = nullableNumber(data.network_epoch), truncated = data.truncated === true
  const heads = data.heads == null ? {} : Object.fromEntries(Object.entries(record(data.heads)).map(([key, head]) => [key, number(head)]))
  // A row names its conversation only; its bot comes from the bot list.
  const bots = list(data.bots ?? []).map(parseOverviewBot)
  const named = (row: Record<string, unknown>) => row.bot != null ? row : { ...row, bot: bots.find((bot) => bot.conversation_id === row.conversation_id) ?? null }
  const tasks = list(data.tasks ?? []).map((task) => parseOverviewTask(named(record(task))))
  const deliveries = list(data.deliveries ?? []).map((delivery) => parseOverviewTask(named({ ...record(delivery), item_kind: "dispatch" })))
  const stops = new Map(deliveries.filter((delivery) => delivery.work_id && (delivery.state === "stop_waiting" || delivery.state === "stop_timed_out")).map((delivery) => [JSON.stringify([delivery.conversation_id, delivery.work_id]), delivery]))
  const mergedTasks = tasks.map((task) => {
    const stop = task.work_id ? stops.get(JSON.stringify([task.conversation_id, task.work_id])) : undefined
    return stop && task.kind !== "check" && !task.outcome && task.state !== "unknown" && task.state !== "unverified" && task.state !== "finished" ? { ...task, state: stop.state, reason: stop.reason ?? task.reason, cwd: task.cwd || stop.cwd } : task
  })
  const extraDeliveries = deliveries.filter((delivery) => {
    if (!delivery.work_id) return true
    const existing = tasks.some((task) => task.conversation_id === delivery.conversation_id && task.work_id === delivery.work_id && (delivery.state === "stop_waiting" || delivery.state === "stop_timed_out" || task.kind === "dispatch"))
    return !existing
  })
  return {
    schema: nullableNumber(data.schema), generated_at_ms: generatedAt, network_epoch: networkEpoch, heads, consistency,
    bots, conversations: list(data.conversations ?? []).map(parseOverviewConversation), tasks: [...mergedTasks, ...extraDeliveries, ...list(data.checks ?? []).map((check) => parseOverviewCheck(named(record(check))))], questions: list(data.questions ?? []).filter((question) => {
      const raw = record(question)
      return raw.answer == null && raw.closed == null && raw.state !== "answered" && raw.state !== "closed"
    }).map(parseOverviewQuestion), jobs: list(data.jobs ?? []).map(parseOverviewJob),
    relations: list(data.relations ?? []).map(parseOverviewRelation), sessions: list(data.sessions ?? []).map(parseOverviewSession), coverage: parseOverviewCoverage(data.coverage, consistency, truncated, generatedAt, networkEpoch), truncated, next_cursor: nullableText(data.next_cursor),
  }
}

/** Keep complete known work records and add observations only present in the bounded overview. Neither read orders the other. */
export function openWorksFromOverview(overview: WorkspaceOverview, openWorks: readonly OpenWorkCard[]): OpenWorkCard[] {
  const works = [...openWorks]
  const known = new Set(works.map((work) => JSON.stringify([work.conversation_id, work.work_id])))
  for (const task of overview.tasks) {
    if ((task.kind !== "work" && task.kind !== "dispatch") || !task.work_id) continue
    const key = JSON.stringify([task.conversation_id, task.work_id])
    if (known.has(key)) continue
    const state: WorkRecord["state"] = task.state === "accepted_waiting_sync" ? "dispatch_waiting"
      : task.state === "unknown" ? "unverified"
        : task.state === "stop_waiting" || task.state === "stop_timed_out" ? "accepted"
          : task.state
    const outcome = task.outcome === "completed" && state === "unverified" ? "unverified" : task.outcome
    works.push({
      work_id: task.work_id, conversation_id: task.conversation_id, target_node: task.target_node ?? "", cwd: task.cwd, title: task.title,
      state, summary: task.summary, outcome, failure_reason: task.reason, ...failure(task.failure), kind: task.work_kind ?? undefined,
      dispatcher: task.dispatcher ?? undefined, call_ref: task.call_ref ?? undefined, continues: task.continues ?? undefined, parent: task.parent ?? undefined,
      thread: task.thread ?? undefined, thread_id: task.thread_id ?? undefined, turn_id: task.turn_id ?? undefined, phase: task.phase ?? undefined,
      ...(task.coordination_job === undefined ? {} : { coordination_job: task.coordination_job }),
      ...(task.state === "stop_waiting" || task.state === "stop_timed_out" ? { stop: task.state } : {}),
    })
    known.add(key)
  }
  return works
}

/** A pushed change: the records it touched, or null when it carries none. */
export function parseRecords(value: unknown): ConversationRecords {
  const data = record(value)
  const last = data.last_message == null ? null : record(data.last_message)
  const scheduler = schedulerSnapshot({ jobs: data.jobs, scope_jobs: data.scope_jobs, unanswered: data.unanswered })
  const id = text(data.conversation_id)
  if (scheduler?.scope_jobs.some((job) => job.conversation_id !== id)) throw new Error('目标任务观察属于别的对话。')
  return {
    conversation_id: id, messages: list(data.messages).map(message), works: list(data.works).map(work),
    last_message: last && { text: text(last.text), role: text(last.role), created_at_ms: number(last.created_at_ms) }, busy: busy(data.busy),
    questions: list(data.questions).map(question), notifications: parseNotifications(data),
    deliveries: list(data.deliveries), checks: list(data.checks).map(coordinationCheck),
    jobs: scheduler?.jobs ?? [], unanswered: scheduler?.unanswered ?? [],
    scope_jobs: scheduler?.scope_jobs ?? [],
    ...(data.context === undefined ? {} : { context: contextProjection(data.context) }),
    ...(data.memory === undefined ? {} : { memory: list(data.memory).map(parseMemoryEntry) }),
    ...(data.memory_staged_count === undefined ? {} : { memory_staged_count: data.memory_staged_count === null ? null : number(data.memory_staged_count) }),
    ...(data.memory_conflicts === undefined ? {} : { memory_conflicts: memoryConflicts(data.memory_conflicts) }),
    ...(data.memory_sync_conflicts === undefined ? {} : { memory_sync_conflicts: parseMemorySyncConflicts(data.memory_sync_conflicts) }),
    sent: list(data.sent).map(sentBotMessage), sent_coverage: sentCoverage(data.sent_coverage), open_works: list(data.open_works).map(openWork),
  }
}
export function parseConversation(value: unknown, expectedId?: string): ConversationPayload {
  const data = record(value)
  const owner = data.conversation === null ? null : conversation(data.conversation)
  if (expectedId && owner?.conversation_id !== expectedId) throw new Error("设备返回了别的对话，已暂停。")
  const scheduler = schedulerSnapshot(data.scheduler)
  if (owner && scheduler?.scope_jobs.some((job) => job.conversation_id !== owner.conversation_id)) throw new Error('目标任务观察属于别的对话。')
  return {
    conversation: owner, messages: list(data.messages).map(message).filter(mainMessage),
    works: withDeliveries(list(data.works).map(work), data.deliveries),
    scheduler: scheduler && { ...scheduler, jobs: scheduler.jobs.filter((job) => job.scope.kind === 'main'), ...(scheduler.context && scheduler.context.scope.kind !== 'main' ? { context: null } : {}) },
    scopeJobs: scheduler?.scope_jobs ?? [],
    checks: list(data.checks).map(coordinationCheck),
    local_node: text(data.local_node),
    changes: list(data.changes).map(changeRecord),
    overwritten: list(data.overwritten).map(overwritten),
    bot_settings: botSettings(data.bot_settings),
    questions: list(data.questions).map(question).filter(mainQuestion),
    sessions: list(data.sessions).map(sessionHandoff),
    sent: list(data.sent).map(sentBotMessage), sent_coverage: sentCoverage(data.sent_coverage),
  }
}
function edit(value: unknown): FieldEdit | undefined {
  if (value == null) return undefined
  const data = record(value)
  return { before: text(data.before), after: text(data.after) }
}
function provider(value: unknown): ProviderId {
  if (!providerIds.includes(value as ProviderId)) throw new Error("模型的 provider 无法识别。")
  return value as ProviderId
}
function modelEdit(value: unknown): ModelEdit | undefined {
  const field = edit(value)
  return field && { provider: provider(record(value).provider), ...field }
}
function botModels(value: unknown): BotModels {
  return Object.fromEntries(Object.entries(record(value)).map(([key, model]) => [provider(key), text(model)]))
}
function avatar(value: unknown): Avatar | null {
  if (value == null) return null
  const data = record(value)
  const outline = OUTLINES.find((item) => item === data.outline), eyes = EYES.find((item) => item === data.eyes), hue = HUES.find((item) => item === data.hue)
  if (!outline || !eyes || hue === undefined) throw new Error("Bot 头像无法识别。")
  return { outline, eyes, hue }
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
    const effort = data.reasoning_effort == null ? undefined : (() => { const value = record(data.reasoning_effort); return { provider: provider(value.provider), before: value.before === null ? null : text(value.before), after: value.after === null ? null : text(value.after) } })()
    const skills = data.skills == null ? undefined : { before: skillSet(record(data.skills).before), after: skillSet(record(data.skills).after) }
    const face = data.avatar == null ? undefined : { before: avatar(record(data.avatar).before), after: avatar(record(data.avatar).after) }
    if (!name && !instructions && !model && !effort && !skills && !face) throw new Error("Bot 修改记录是空的。")
    return { kind: "profile", ...(name ? { name } : {}), ...(instructions ? { instructions } : {}), ...(model ? { model } : {}), ...(effort ? { reasoning_effort: effort } : {}), ...(skills ? { skills } : {}), ...(face ? { avatar: face } : {}) }
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
function overwritten(value: unknown): Overwritten {
  const data = record(value)
  const base = { change_id: text(data.change_id), by: text(data.by) }
  switch (data.field) {
    case "name": case "instructions": return { ...base, field: data.field, value: text(data.value) }
    case "model": return { ...base, field: "model", provider: provider(data.provider), value: text(data.value) }
    case "reasoning_effort": return { ...base, field: "reasoning_effort", provider: provider(data.provider), value: data.value === null ? null : text(data.value) }
    case "skills": return { ...base, field: "skills", value: skillSet(data.value) }
    case "avatar": return { ...base, field: "avatar", value: avatar(data.value) }
    default: throw new Error("被覆盖的设置无法识别。")
  }
}
function botSettings(value: unknown): BotSettings {
  const data = record(value)
  return {
    skills: list(data.skills).map((item) => { const skill = record(item); return { skill_id: text(skill.skill_id), name: text(skill.name), description: text(skill.description) } }),
    schedules: list(data.schedules).map((item) => {
      const view = record(item)
      return { ...schedule(view), schedule_id: text(view.schedule_id), next_run_at_ms: view.next_run_at_ms == null ? null : number(view.next_run_at_ms) }
    }),
    memory: list(data.memory).map(parseMemoryEntry),
    memory_sync_conflicts: parseMemorySyncConflicts(data.memory_sync_conflicts),
    ...(data.memory_staged_count === undefined ? {} : { memory_staged_count: data.memory_staged_count === null ? null : number(data.memory_staged_count) }),
    ...(data.memory_conflicts === undefined ? {} : { memory_conflicts: memoryConflicts(data.memory_conflicts) }),
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
  return { ...parseCreateBot(value), models: botModels(data.models), reasoning_efforts: botModels(data.reasoning_efforts ?? {}), skills: skillSet(data.skills), avatar: avatar(data.avatar), avatar_version: data.avatar_version == null ? null : text(data.avatar_version), event_hash: text(data.event_hash), last_message: last && { text: text(last.text), role: text(last.role), created_at_ms: number(last.created_at_ms) }, busy: busy(data.busy) }
}
const busy = (value: unknown): Busy => value === "thinking" || value === "replying" ? value : null
/** Bots in order; `successor` marks the one that becomes primary once the primary bot is deleted. */
export function parseBots(value: unknown): BotRecord[] {
  const data = record(value)
  return list(data.bots).map((item) => { const bot = parseBot(item); return bot.conversation_id === data.successor ? { ...bot, successor: true } : bot })
}
/** The primary bot's conversation: sessions no bot started go there first. */
export function parsePrimary(value: unknown): string | null {
  const primary = record(value).primary
  return primary == null ? null : text(primary)
}
export function parsePendingBot(value: unknown): PendingBot {
  const data = record(value), submission = record(data.submission)
  if (submission.state !== "unconfirmed" && submission.state !== "rejected") throw new Error("Bot 创建记录的提交状态无效。")
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
  return { node: text(data.node), agent: data.agent === null ? null : provider(data.agent), session_id: text(data.session_id), cwd: text(data.cwd), summary: text(data.summary), ...(data.request_id == null ? {} : { request_id: text(data.request_id) }) }
}
function closeReason(value: unknown): CloseReason {
  if (value !== "request_lost") throw new Error("问题关闭的原因无法识别。")
  return value
}
function parseAttention(value: unknown): NonNullable<Question['attention']> {
  const data = record(value)
  if (data.kind !== 'question' && data.kind !== 'approval' && data.kind !== 'browser_takeover') throw new Error('请求类型无法识别。')
  return { kind: data.kind, device_id: data.device_id === null ? null : text(data.device_id) }
}
function question(value: unknown): Question {
  const data = record(value)
  const bot = data.bot == null ? null : record(data.bot), answer = data.answer == null ? null : record(data.answer)
  return {
    question_id: text(data.question_id), conversation_id: text(data.conversation_id), ...(data.target == null ? {} : { target: parseInputTarget(data.target) }),
    bot: bot && { bot_id: text(bot.bot_id), name: text(bot.name) },
    severity: severity(data.severity), attention: data.attention == null ? undefined : parseAttention(data.attention), text: text(data.text),
    choices: list(data.choices ?? []).map(questionChoice),
    about: subject(data.about), author: text(data.author), created_at_ms: number(data.created_at_ms),
    answer: answer && { message_id: text(answer.message_id), text: text(answer.text), author: text(answer.author), created_at_ms: number(answer.created_at_ms) },
    closed: data.closed == null ? null : closeReason(data.closed),
  }
}
function questionChoice(value: unknown): QuestionChoice {
  const data = record(value)
  if (data.action == null) return { label: text(data.label) }
  if (data.action !== 'allow_once' && data.action !== 'deny') throw new Error('问题选项的动作无法识别。')
  return { label: text(data.label), action: data.action }
}
function sentBotMessage(value: unknown): SentBotMessage {
  const data = record(value)
  return { message_id: text(data.message_id), to: text(data.to), text: text(data.text), created_at_ms: number(data.created_at_ms) }
}
function sessionHandoff(value: unknown): SessionHandoff {
  const data = record(value)
  return { node: text(data.node), agent: provider(data.agent), session_id: text(data.session_id), cwd: text(data.cwd), summary: text(data.summary), muted: data.muted === true, created_at_ms: number(data.created_at_ms) }
}
/** Every unanswered question across the network. */
export function parseQuestions(value: unknown): Question[] {
  return list(record(value).questions).map(question)
}
/** The answer that stands for this question, whether this browser's or one written first. */
export function parseAnswer(value: unknown, questionId: string): AnswerResult {
  const data = record(value), message = record(data.message), status = data.status
  if (status !== "answered" && status !== "already_answered") throw new Error("回答结果无法识别。")
  if (message.message_id !== answerId(questionId)) throw new Error("回答结果和这个问题对不上。再查一次。")
  return { status, text: text(message.text) }
}
function outlet(value: unknown): Outlet {
  if (value !== "record" && value !== "both") throw new Error("提醒去向无法识别。")
  return value
}
export function parseTriageSettings(value: unknown): TriageSettings {
  const data = record(value), outbox = record(data.outbox), status = record(data.status), budget = record(status.budget)
  if (budget.paused_reason !== null && budget.paused_reason !== "budget_exhausted") throw new Error("分诊暂停原因无法识别。")
  return {
    default_outlet: outlet(data.default_outlet),
    projects: list(data.projects).map((item) => { const project = record(item); return { prefix: text(project.prefix), outlet: outlet(project.outlet) } }),
    webhook_url: data.webhook_url == null ? "" : text(data.webhook_url), scan_codex_sessions: flag(data.scan_codex_sessions),
    codex_sessions: data.codex_sessions === null ? null : text(data.codex_sessions),
    webhook_secret_set: flag(data.webhook_secret_set), decision_url: text(data.decision_url), device_id: text(data.device_id),
    decision_tokens: list(data.decision_tokens).map((item) => { const token = record(item); return { token_id: text(token.token_id), label: text(token.label), created_ms: number(token.created_ms) } }),
    outbox: { pending: number(outbox.pending), delivered: number(outbox.delivered), dead: number(outbox.dead) },
    status: {
      budget: { day: text(budget.day), used: number(budget.used), limit: number(budget.limit), retry_at_ms: number(budget.retry_at_ms), paused_reason: budget.paused_reason },
      pending_events: number(status.pending_events), inflight_events: number(status.inflight_events), unknown_events: number(status.unknown_events),
    },
  }
}
