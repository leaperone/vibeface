import type { ProviderId, ProviderReadiness } from "../lib/node.ts"
import type { PendingMessage, ReplyStatus, TranscriptMessage } from "../chat/types.ts"
import type { FailureKind } from "../lib/errors.ts"
import type { Avatar } from "../avatar/face.ts"
import type { Attachment } from "../chat/types.ts"

export type InputTarget =
  | { kind: 'bot_main'; bot_id: string }
  | { kind: 'task'; task_id: string }
  | { kind: 'work'; work_id: string }
  | { kind: 'routine'; bot_id: string; trigger: string }
  | { kind: 'channel_member'; channel_id: string; bot_id: string }
  | { kind: 'channel_task'; channel_id: string; task_id: string; bot_id: string }
export type InputIntent = 'continue' | 'new_task' | 'defer'
export type InputOptions = { target: InputTarget; intent?: InputIntent; expected_revision?: number; conversation_id?: string; onSaved?: (input: PendingMessage) => void }
export type SessionScope = { kind: 'main' } | { kind: 'workspace_task'; task_id: string } | { kind: 'task'; work_id: string } | { kind: 'routine'; trigger: string }
export type InputReceipt = {
  message_id: string; target: InputTarget
  stage: 'entry_accepted' | 'owner_accepted' | 'submitting' | 'provider_accepted' | 'associated' | 'refused' | 'unknown'
  actual_turn_id: string | null; reason: string | null; retry_safe: boolean; control_event: string | null
}
export type ScopeContext = { conversation_id: string; target: InputTarget } & (
  | { available: true; context: ContextProjection; reason: null }
  | { available: false; context: null; reason: string | null }
)
export type WorkspaceFilter = { channel_id?: string; task_id?: string; limit?: number }
export type WorkspaceChannel = {
  channel_id: string; title: string; purpose: string; members: string[]; coordinator_bot: string; coordinator_node: string
  owner_epoch: number; revision: number; event_hash: string; created_at_ms: number
}
export type WorkspaceTaskAction = 'continue' | 'stop' | 'cancel' | 'defer'
export type WorkspaceTask = {
  task_id: string; channel_id?: string; root_task_id: string; parent_task_id?: string; owner_bot: string; title: string
  request: string; expected_result: string; source_events: string[]; dependencies: string[]; reply_only: boolean; revision: number
  action: WorkspaceTaskAction; request_event: string; event_hash: string; created_at_ms: number; updated_at_ms: number
  state: 'not_started' | 'running' | 'waiting_dependencies' | 'waiting_user' | 'update_pending' | 'stop_pending' | 'needs_reconciliation' | 'completed' | 'cancelled' | 'deferred'
  waiting_reason?: string; current_assignment?: string
}
export type WorkspaceAssignment = {
  assignment_id: string; task_id: string; started_revision: number; bot_id: string; execution_node: string
  bot_owner_epoch: number; channel_owner_epoch?: number; work_id?: string; basis: Record<string, number>; resource_roots: string[]; automatic: boolean
  accepted_revision?: number; adopted_revision?: number; completed_revision?: number
  trigger_message_id?: string; source_event?: string; submitted_result?: { revision: number; text: string; event_hash: string }
  state: 'proposed' | 'accepted' | 'adopted' | 'completed' | 'stopped' | 'unknown'
  result?: string; event_hash: string; created_at_ms: number
}
export type InputControlAction = { kind: 'release' } | { kind: 'cancel' } | { kind: 'edit'; text: string; attachments: Attachment[] } | { kind: 'move'; before_message_id?: string }
export type QueuedInput = {
  input: Omit<WorkspaceInputBody, 'type'>; conversation_id: string; source_event: string; control_event: string
  effective_order: number; deferred: boolean; cancelled: boolean
}
export type WorkspaceCommand =
  | { kind: 'create_channel'; channel_id: string; title: string; purpose: string; members: string[]; coordinator_bot: string; coordinator_node: string }
  | { kind: 'configure_channel'; channel_id: string; expected_revision: number; title: string; purpose: string; members: string[]; coordinator_bot: string; coordinator_node: string; owner_epoch: number }
  | { kind: 'post_channel_message'; channel_id: string; message_id: string; text: string; attachments: Attachment[]; recipient_bot?: string; task_id?: string; reply_to?: string }
  | { kind: 'create_task'; task_id: string; channel_id?: string; parent_task_id?: string; owner_bot: string; title: string; request: string; expected_result: string; source_events: string[]; dependencies: string[]; reply_only: boolean }
  | { kind: 'revise_task'; task_id: string; expected_revision: number; request: string; expected_result: string; dependencies: string[] }
  | { kind: 'control_task'; task_id: string; expected_revision: number; action: WorkspaceTaskAction }
  | { kind: 'assign_task'; task_id: string; expected_revision: number; assignment_id: string; bot_id: string; execution_node: string; bot_owner_epoch: number; channel_owner_epoch?: number; work_id?: string; resource_roots: string[]; automatic: boolean }
  | { kind: 'assignment_progress'; assignment_id: string; revision: number; state: WorkspaceAssignment['state']; input_event?: string; evidence_event?: string; execution?: TurnRuntime['execution']; result?: string }
  | { kind: 'control_input'; message_id: string; expected_event: string; action: InputControlAction }
export type WorkspaceDomainState = { channels: WorkspaceChannel[]; tasks: WorkspaceTask[]; assignments: WorkspaceAssignment[]; inputs: QueuedInput[]; coverage: { limit: number; truncated: boolean } }
export type WorkspaceCommandResult = Omit<WorkspaceDomainState, 'coverage'> & { command_id: string; event_hash: string; duplicate: boolean }
export type WorkspaceCommandNotSaved = { accepted: false; code: 'workspace_command_not_saved'; stored: false; command_id: string; reason: string }
export type WorkspaceInputBody = {
  type: 'input_posted'; message_id: string; target: InputTarget; intent: InputIntent; sequence: number; expected_revision?: number
  text: string; attachments?: Attachment[]; report_results: boolean
}
export type WorkspaceHistoryBody =
  | WorkspaceInputBody
  | { type: 'workspace_commanded'; command_id: string; command: WorkspaceCommand }
  | { type: 'message_posted'; message_id: string; role: string; text: string; attachments?: Attachment[]; in_reply_to?: string; status?: ReplyStatus; failure?: FailureKind }
  | { type: 'work_finished'; work_id: string; outcome: WorkOutcome; result_text: string; failure_reason?: string; failure?: FailureKind }
  | { type: 'work_dispatched'; work_id: string; title: string; prompt: string }
  | { type: 'work_progress'; work_id: string; phase: WorkPhase; summary: string; thread_id?: string; turn_id?: string }
export type WorkspaceSpeaker = { kind: 'human'; device_id: string } | { kind: 'bot'; bot_id: string } | { kind: 'node'; device_id: string }
export type WorkspaceHistoryItem = { event_hash: string; created_at_ms: number; scope_id: string; kind: string; body: WorkspaceHistoryBody; speaker: WorkspaceSpeaker }
export type WorkspaceHistory = { events: WorkspaceHistoryItem[]; coverage: { limit: number; truncated: boolean; scope: 'task' | 'channel' | 'workspace' } }

export type NodeInfo = {
  device_id: string
  name: string
  local: boolean
  online: boolean
  revoked: boolean
  capabilities?: { manager: string | null; manager_state: BotReadiness | null; manager_readiness?: ProviderReadiness | null; relay: boolean; roots: string[]; codex_connected?: boolean }
}
/** `unverified` remains runnable while authentication or model availability has no evidence. */
export type BotReadiness = "ready" | "starting" | "needs_setup" | "unverified"
export type ConversationSummary = { conversation_id: string; title: string; owner: string | null; owner_epoch: number }
/** Where a work stands in the signed DAG. */
export type WorkState = "accepted" | "running" | "interrupting" | "finished" | "unverified"
/** The phase recorded by a WorkProgress event. */
export type WorkPhase = "started" | "running" | "interrupting" | "finishing"
/** A dispatch this device still carries: the target has not taken it, refused it, or was removed before saying. Not in the DAG yet. */
export type DispatchState = "dispatch_waiting" | "dispatch_rejected" | "dispatch_unverified"
/** A stop this device still carries: its device has not confirmed it, or the deadline passed. */
export type StopState = "stop_waiting" | "stop_timed_out"
export type WorkOutcome = "completed" | "failed" | "interrupted" | "rejected" | "unverified"
export type ResolvedModel = { provider: ProviderId; model: string | null }
type WorkFields = {
  work_id: string
  target?: InputTarget
  trigger: string
  target_node: string
  cwd: string
  /** A few words for the user; `prompt` is what the agent was asked to do. */
  title: string
  prompt: string
  summary: string
  outcome: WorkOutcome | null
  result_text: string
  failure_reason: string | null
  /** Why it failed, as a kind; `failure_reason` keeps the original text. */
  failure?: FailureKind
  /** A provider: a turn on it that its device watches; `task`: a task run by its device's coordination Agent. */
  kind?: ProviderId | "task"
  /** The dispatcher's optional model override. */
  model?: string
  /** The Codex thread and turn running it; works steered into one turn share both. */
  thread_id?: string
  turn_id?: string
  /** A stop sent while the device was unreachable: still retrying, or past its deadline. */
  stop?: StopState
  coordination_job?: ScopeJob | null
}
type WorkRelations = {
  dispatcher: string
  call_ref: string
  continues: string | null
  parent: string | null
  thread: string | null
  phase: WorkPhase | null
}
export type WorkRecord = WorkFields & (
  | (WorkRelations & { state: WorkState; resolved: ResolvedModel })
  | (Partial<WorkRelations> & { state: DispatchState; resolved?: never })
)
/** The fields a task card can read from either full work or an overview summary. */
export type WorkCard = Pick<WorkRecord, "work_id" | "target" | "target_node" | "cwd" | "title" | "state" | "summary" | "outcome" | "failure_reason" | "failure" | "kind" | "dispatcher" | "call_ref" | "continues" | "parent" | "thread" | "thread_id" | "turn_id" | "phase" | "stop">
export type OpenWorkCard = WorkCard & { conversation_id: string }
export const providerStages = ['validate', 'install', 'launch', 'authentication', 'model_list', 'session_start', 'turn_start', 'run', 'read_back', 'interrupt', 'compact', 'handoff', 'record'] as const
export type ProviderStage = typeof providerStages[number]
export type TurnRuntime = {
  provider: ProviderId; model: string | null; reasoning_effort: string | null; generation: string; session_generation: number; delivery_id: string
  stage: ProviderStage; execution: 'not_started' | 'started' | 'unknown'; attempted_at_ms: number
  switched_from: { provider: ProviderId; model: string | null; reasoning_effort: string | null } | null; switch_reason: FailureKind | null
}
export type ManagerJob = {
  trigger: string
  state: string
  scope: SessionScope
  into?: { provider: ProviderId; attempt_id: string; thread_id: string; turn_id: string }
  receipt?: { accepted_at_ms: number; actual_turn_id: string | null }
  uncertain?: boolean
  context?: ContextProjection | null
  lane?: AgentLane
  hold_reason?: QueueHoldReason
  message_id?: string
  reply_message_id?: string
  /** A failed or interrupted reply's cause; `detail` is the original text, for 技术详情. */
  failure?: { kind: FailureKind; detail: string }
  runtime?: TurnRuntime | null
  retryable?: boolean
  pending_check?: boolean
  /** How the reply a steered message joined ended. */
  outcome?: SteerOutcome
  /** The device that reported this observation. */
  source_node?: string
}
export type ScopeJob = {
  conversation_id: string; trigger: string; scope: SessionScope; state: string; sequence: number
  kind: 'human' | 'answer' | 'work_result' | 'agent_task' | 'workspace_task' | 'schedule' | 'bot_message'
  lane: AgentLane; source_node: string; owner: string | null; message_id: string | null; reply_message_id: string
  hold_reason?: QueueHoldReason
}
export type AgentLane = 'main' | 'task' | 'schedule_routine' | 'bot_message'
export type QueueHoldReason = 'active_turn' | 'steering_settlement' | 'provider_serial' | 'task_limit' | 'machine_slot' | 'pending_check' | 'backend_config_pending' | 'attachment_preparation' | 'earlier_input' | 'resource_conflict'
export type ContextProjection = {
  scope: SessionScope
  sources: { kind: 'trigger' | 'message' | 'work' | 'dispatch' | 'checkpoint' | 'handoff' | 'memory' | 'question' | 'attachment'; event_id?: string; author?: string; seq?: number; reference?: string }[]
  sources_omitted: number
  inputs: { message_id: string; source_event: string; stage: InputReceipt['stage']; actual_turn_id: string | null; expected_turn_id: string; accepted_at_ms?: number }[]
  inputs_omitted: number
  attempt_id: string
  provider: ProviderId
  stable_fingerprint: string
  dynamic_fingerprint: string
  session_generation: number
  coverage: {
    messages_omitted: number; works_omitted: number; dispatches_omitted: number
    questions_omitted: number; attachments_omitted: number; memory_omitted: number
    checkpoint_omitted: number; handoff_omitted: number; pending_checks_omitted: number
    bot_indexes_omitted: Record<string, number>
    history_retrieval: string
  }
  stable_bytes: number
  dynamic_bytes: number
  invalidation_reason: string | null
  stage: 'compiled' | 'started' | 'completed' | 'failed' | 'cancelled' | 'interrupted' | 'unknown'
  session_status?: { resumable: boolean; pending_generation: boolean } | null
}
export type MemoryConflict = {
  call_ref: string; memory_id: string; expected_epoch: number; actual_epoch: number
  expected_revision: string; actual_revision: string; reason: string
}
export type ConversationMemoryConflict = MemoryConflict & { history_id: string }
export type SteerOutcome = 'answered' | 'unfinished' | 'unknown'
export type SchedulerSnapshot = {
  jobs: ManagerJob[]
  local_jobs: ManagerJob[]
  scope_jobs: ScopeJob[]
  context?: ContextProjection | null
  /** Messages a former scheduling device left unanswered; only listed on the device that schedules now. */
  unanswered: string[]
  provider?: string
  model?: string
  capabilities?: { streaming: boolean; cancel: boolean; steer: boolean }
  /** Set when another device schedules this conversation; unreachable leaves its replies' state unknown. */
  remote?: { owner: string; reachable: boolean; jobs: ManagerJob[]; scope_jobs: ScopeJob[]; memory_conflicts: ConversationMemoryConflict[] | null }
}
/** A coordination process still alive after its turn ended; the Node closes it once the process exits. */
export type CoordinationCheck = {
  check_id: string
  conversation_id: string
  trigger: string
  attempt_id: string
  detail: string
  process_groups: number[]
  created_at_ms: number
}
/** Why a work waits for the person: its device is offline, its App Server disconnected, or its result could not be checked. */
export type ConfirmReason = "offline" | "disconnected" | "unverified"
/** A work no result has settled yet, from any conversation. */
export type OpenWork = WorkRecord & { conversation_id: string }
export type WorkDetailWork = Omit<OpenWork, 'target'> & { target: InputTarget | null; source_events: string[] }
export type WorkDetail = { work: WorkDetailWork; coverage: { source: 'shared_history'; complete: false } }
/** What one change touched, pushed by the Node so the page merges it without a reload. */
export type ConversationRecords = {
  conversation_id: string
  history_authority: string
  refresh_metadata: boolean
  messages: TranscriptMessage[]
  works: WorkRecord[]
}
/**
 * What only the person can settle; nothing is resent.
 * `work`: a work waiting for the person's confirmation, answered completed or interrupted. `device`: several works one offline
 * device or one disconnected Codex cannot report, settled one by one on a single card. `process`: a coordination process the person confirms stopped.
 */
export type PendingCheck =
  | { kind: "work"; check_id: string; detail: string; work_id: string; target?: InputTarget; target_node: string; conversation_id: string; title: string; reason: ConfirmReason }
  | { kind: "device"; check_id: string; detail: string; target_node: string; reason: "offline" | "disconnected"; works: Extract<PendingCheck, { kind: "work" }>[] }
  | { kind: "process"; check_id: string; detail: string }
export type NotificationRecord = { notification_id: string; kind: string; text: string; conversation_id: string; target?: InputTarget }
export const sessionNotificationKind = "session_attributed"
export const isSessionNotification = (item: Pick<NotificationRecord, "kind">) => item.kind === sessionNotificationKind
export const isMainNotification = (item: Pick<NotificationRecord, 'target'>, botId: string | undefined) => botId !== undefined && item.target?.kind === 'bot_main' && item.target.bot_id === botId
export type FieldEdit = { before: string; after: string }
/** A bot's model on one provider; empty follows the device's default. */
export type ModelEdit = FieldEdit & { provider: ProviderId }
/** A provider without an entry runs the device's default model. */
export type BotModels = Partial<Record<ProviderId, string>>
export type BotSkill = { name: string; description: string; body: string }
/** Runs at `first_run_at_ms`, then every `every_minutes` when set; without it, once. `depth` > 0 is a reminder the bot set itself. */
export type BotSchedule = { name: string; prompt: string; first_run_at_ms: number; every_minutes?: number; enabled: boolean; depth?: number }
export type MemoryVersion = { revision: string; control_revision: number; source_event_hash: string }
export type MemoryBase = { epoch: number; version: MemoryVersion | null }
export type MemoryMetadata = MemoryVersion & {
  epoch: number; origin: 'agent' | 'user'; user_controlled: boolean; essential: boolean
}
export type MemoryState = { memory_id: string; text: string | null; metadata: MemoryMetadata }
export type MemoryEntry = MemoryState & { text: string }
export type MemoryControlResult = {
  state: 'applied' | 'already_recorded' | 'superseded' | 'conflict'
  operation_id: string; current: MemoryState | null
}
export type MemoryStateResult = {
  current: MemoryState | null; receipt: null | 'recorded' | 'superseded' | 'not_seen_here'
}
export type MemorySyncConflict = {
  memory_id: string; losing_revision: string; winning_revision: string
  losing_event_hash: string; winning_event_hash: string
  reason: 'user_protected' | 'concurrent_automatic' | 'concurrent_user'
}
export type MemorySyncConflicts = { items: MemorySyncConflict[]; has_more: boolean }
/** null: every shared skill, later ones included. */
export type SkillSet = string[] | null
type SkillsEdit = { before: SkillSet; after: SkillSet }
/** null: the face its id gives. */
export type AvatarEdit = { before: Avatar | null; after: Avatar | null }
/** `before` null is a new item, `after` null a removed one (packages/docs/design/bot-config.md section 2). */
export type BotChange =
  | { kind: "profile"; name?: FieldEdit; instructions?: FieldEdit; model?: ModelEdit; reasoning_effort?: { provider: ProviderId; before: string | null; after: string | null }; skills?: SkillsEdit; avatar?: AvatarEdit }
  | { kind: "skill"; skill_id: string; before: BotSkill | null; after: BotSkill | null }
  | { kind: "schedule"; schedule_id: string; before: BotSchedule | null; after: BotSchedule | null }
  | { kind: "memory"; memory_id: string; before: string | null; after: string | null }
/** The person's signed message a turn stood on; it may be in another bot's chat. */
type ChangeCause = { conversation_id: string; message_id: string }
/** A change to the bot of `conversation_id`, applied from `cause`; null when the bot's own turn made it with no message behind it. */
export type BotChangeRecord = { change_id: string; conversation_id: string; cause: ChangeCause | null; event_hash: string; created_at_ms: number; change: BotChange }
/** A chat change another device replaced without seeing it; `value` is the setting now, which `by` set. */
export type Overwritten = { change_id: string; by: string } & (
  | { field: "name" | "instructions"; value: string }
  | { field: "model"; provider: ProviderId; value: string }
  | { field: "reasoning_effort"; provider: ProviderId; value: string | null }
  | { field: "skills"; value: SkillSet }
  | { field: "avatar"; value: Avatar | null })
export type SkillSummary = { skill_id: string; name: string; description: string }
export type ScheduleView = BotSchedule & { schedule_id: string; next_run_at_ms: number | null }
export type ScheduleAction = "pause" | "resume" | "delete"
/** This bot's schedules still ahead, its notes, and the skill library every bot shares. */
export type BotSettings = { skills: SkillSummary[]; schedules: ScheduleView[]; memory: MemoryEntry[]; memory_sync_conflicts: MemorySyncConflicts; memory_staged_count?: number | null; memory_conflicts?: ConversationMemoryConflict[] | null }
export type ConversationPayload = {
  history_authority: string
  history: ConversationHistory
  deliveries: DeliveryRecord[]
  conversation: ConversationSummary | null
  messages: TranscriptMessage[]
  works: WorkRecord[]
  scheduler: SchedulerSnapshot | null
  /** Public job metadata for selecting a registered target; Main's timeline uses scheduler.jobs. */
  scopeJobs: ScopeJob[]
  checks: CoordinationCheck[]
  local_node: string
  /** Oldest first. */
  changes: BotChangeRecord[]
  overwritten: Overwritten[]
  bot_settings: BotSettings
  /** This chat's questions, answered ones included. */
  questions: Question[]
  sessions: SessionHandoff[]
  /** What this bot wrote to other bots; `to` is the receiver's conversation. */
  sent: SentBotMessage[]
  sent_coverage: SentCoverage
}
export type SentCoverage = { scope: 'main'; omitted_unknown: number }
export type SentBotMessage = { message_id: string; to: string; text: string; created_at_ms: number }
/** "chat" while a conversation route is on screen; reading it acknowledges ordinary notifications. */
export type WorkspaceView = "chat" | "tasks" | "other"
/** One version of an event, as a Node keeps it when two copies of a device wrote the same place. */
export type ConflictVersion = { hash: string; scope_id: string; created_at_ms: number; body: Record<string, unknown> }
type LogConflict = { author: string; seq: number; kept: ConflictVersion | null; other: ConflictVersion }
export type WriteState = "aligning" | "read_only" | "writable" | "forked"
export type WriteStatus = { state: WriteState; conflicts: LogConflict[] }
export type { PendingMessage }

export type BotProject = { target_node: string; cwd: string }
export type CreateBotRequest = { bot_id: string; conversation_id: string; name: string; instructions: string; project?: BotProject }
/** A bot conversation's newest message, cut short by the Node for the conversation list. */
type LastMessage = { text: string; role: string; created_at_ms: number }
/** What the bot does on the device serving this list: a message waiting or claimed, or a reply running. A bot another device schedules reads null once its message is delivered. */
export type Busy = "thinking" | "replying" | null
/** `avatar` null: the face its id gives. `avatar_version` names the face to change it, null before any change. */
export type BotRecord = CreateBotRequest & { models: BotModels; reasoning_efforts?: BotModels; skills: SkillSet; avatar: Avatar | null; avatar_version: string | null; event_hash: string; last_message: LastMessage | null; busy: Busy; successor?: true }

export type OverviewBot = Pick<BotRecord, "bot_id" | "conversation_id" | "name">
export type OverviewNode = Pick<NodeInfo, "device_id" | "name" | "online" | "revoked" | "local">
export type OverviewConsistency = "local_snapshot" | "mixed_remote" | "partial"
export type OverviewTaskState = WorkState | DispatchState | StopState | "accepted_waiting_sync" | "unknown"
export type OverviewDeliveryState = DispatchState | "dispatch_accepted" | StopState
export type OverviewTaskKind = "work" | "check" | "dispatch"
export type OverviewNavigation = { bot_id: string; conversation_id: string; work_id?: string }

type OverviewTaskCommon = {
  coordination_job?: ScopeJob | null
  item_id: string
  work_id: string | null
  conversation_id: string
  bot: OverviewBot | null
  title: string
  summary: string
  cwd: string
  state: OverviewTaskState
  outcome: WorkOutcome | null
  target_node: string | null
  dispatcher: string | null
  call_ref: string | null
  work_kind: ProviderId | "task" | null
  parent: string | null
  continues: string | null
  thread: string | null
  thread_id: string | null
  turn_id: string | null
  phase: WorkPhase | null
  reason: string | null
  failure?: FailureKind
  node: OverviewNode | null
  navigation: OverviewNavigation | null
  source: "event" | "dag" | "local_dispatch" | "local_check" | "remote_owner"
}

/** A work fact, a user confirmation check, or a pre-DAG dispatch delivery. */
export type OverviewTask =
  | (OverviewTaskCommon & { kind: "work" })
  | (OverviewTaskCommon & { kind: "check"; check_id: string; work_ids: string[] })
  | (OverviewTaskCommon & { kind: "dispatch"; delivery_state: OverviewDeliveryState })

export type OverviewQuestion = {
  attention?: Question["attention"]
  kind: "question"
  question_id: string
  conversation_id: string
  bot: OverviewBot | null
  text: string
  severity: Severity
  choices: QuestionChoice[]
  request_id: string | null
  work_id: string | null
  about: QuestionSubject | null
  created_at_ms: number
  state: "unanswered"
  navigation: { bot_id: string; conversation_id: string } | null
}

export type OverviewJob = {
  lane?: AgentLane
  hold_reason?: QueueHoldReason
  context?: ContextProjection | null
  job_id: string
  conversation_id: string | null
  trigger: string
  state: string
  runtime?: TurnRuntime | null
  retryable?: boolean
  message_id: string | null
  reply_message_id: string | null
  source_node: string | null
  pending_check: boolean
  outcome: SteerOutcome | null
  unknown_reason: string | null
  kind: string | null
  sequence: number | null
  owner_epoch: number | null
}

export type OverviewRemoteOwner = { owner: string | null; reachable: boolean | null; reason: string | null; status?: string }
export type OverviewCoverage = {
  source: "open_works" | "workspace_overview"
  complete: boolean
  bounded: boolean
  generated_at_ms: number | null
  network_epoch: number | null
  consistency: OverviewConsistency
  truncated: boolean
  local_heads: boolean
  remote_owners: OverviewRemoteOwner[]
  unreachable_nodes: string[]
  omitted_finished: number | null
  omitted_sessions: number | null
  omitted_tasks: number | null
  omitted_questions: number | null
  omitted_checks: number | null
  omitted_dispatches: number | null
  omitted_deliveries: number | null
  omitted_relations: number | null
  omitted_jobs: number | null
  omitted_live_progress: boolean
}

export type OverviewRelation = { from_work: string; to_work: string; kind: "parent" | "continues" | "dispatches" }
export type OverviewConversation = { conversation_id: string; title: string; owner: string | null; owner_epoch: number }
export type OverviewSession = { conversation_id: string; node: string; session_id: string; agent: ProviderId; muted: boolean; created_at_ms: number; version: string; cwd: string; summary: string }

/** A finite read-only projection. It never implies a complete global queue. */
export type WorkspaceOverview = {
  schema: number | null
  generated_at_ms: number | null
  network_epoch: number | null
  heads: Record<string, number>
  consistency: OverviewConsistency
  bots: OverviewBot[]
  conversations: OverviewConversation[]
  tasks: OverviewTask[]
  questions: OverviewQuestion[]
  jobs: OverviewJob[]
  sessions: OverviewSession[]
  relations: OverviewRelation[]
  coverage: OverviewCoverage
  truncated: boolean
  next_cursor: string | null
}

export type WorkspaceActivityGroup = "attention" | "working" | "waiting"
export type WorkspaceActivityJob = { conversation_id: string | null; job: ManagerJob | OverviewJob }
export type WorkspaceActivityFacts = {
  questions: Question[]
  checks: PendingCheck[]
  overviewQuestions: OverviewQuestion[]
  overviewChecks: Extract<OverviewTask, { kind: "check" }>[]
  jobs: (ManagerJob | OverviewJob)[]
}
export type WorkspaceActivityAttention = Question | PendingCheck | OverviewQuestion | Extract<OverviewTask, { kind: "check" }>
type WorkspaceActivityBase = {
  id: string
  conversation_id: string | null
  work_id: string | null
  group: WorkspaceActivityGroup
  state: string
  facts: WorkspaceActivityFacts
}
export type WorkspaceActivity =
  | (WorkspaceActivityBase & { kind: "work"; work: OpenWorkCard; projection?: Extract<OverviewTask, { kind: "work" | "dispatch" }> })
  | (WorkspaceActivityBase & { kind: "turn"; job: ManagerJob | OverviewJob })
  | (WorkspaceActivityBase & { kind: "attention"; attention: WorkspaceActivityAttention })
export type WorkspaceActivityView = {
  activities: WorkspaceActivity[]
  groups: Record<WorkspaceActivityGroup, WorkspaceActivity[]>
}

export type PendingBot = CreateBotRequest & {
  submission: { state: "unconfirmed" | "rejected"; entry_node: string }
}

export type Severity = "info" | "attention" | "urgent"
/** The device, project and session a question is about. `agent` is null when the question is about the device's own settings. */
export type QuestionSubject = { node: string; agent: ProviderId | null; session_id: string; cwd: string; summary: string; request_id?: string }
export type QuestionChoice = { label: string; action?: 'allow_once' | 'deny' }
type QuestionAnswer = { message_id: string; text: string; author: string; created_at_ms: number }
/** Why the device that asked closed a question unanswered: the App Server request it held is gone. */
export type CloseReason = "request_lost"
/** Something only the person can settle, asked in a bot's chat. Text answers are always allowed; a choice answers with its label. */
export type Question = {
  question_id: string
  conversation_id: string
  target?: InputTarget
  bot: { bot_id: string; name: string } | null
  severity: Severity
  attention?: { kind: "question" | "approval" | "browser_takeover"; device_id: string | null }
  text: string
  choices: QuestionChoice[]
  about: QuestionSubject
  author: string
  created_at_ms: number
  answer: QuestionAnswer | null
  /** Set only while unanswered: an answer that stands wins over a close. */
  closed: CloseReason | null
}
/** `already_answered`: another answer stood first; `text` is the one that stands. */
export type AnswerResult = { status: "answered" | "already_answered"; text: string }
/** An answer this browser sent whose outcome it has not seen; trying again sends the same signed request. */
export type PendingAnswer = { question_id: string; text: string }
/** A session no bot started that this bot looks after; `muted`: the person stopped following it here. */
export type SessionHandoff = { rank?: string; node: string; agent: ProviderId; session_id: string; cwd: string; summary: string; muted: boolean; created_at_ms: number }
/** Where questions go besides the web: "both" also sends them to Grok Bot. */
export type Outlet = "record" | "both"
export type TriageSettings = {
  default_outlet: Outlet
  projects: { prefix: string; outlet: Outlet }[]
  webhook_url: string
  scan_codex_sessions: boolean
  codex_sessions: string | null
  webhook_secret_set: boolean
  decision_url: string
  device_id: string
  decision_tokens: { token_id: string; label: string; created_ms: number }[]
  outbox: { pending: number; delivered: number; dead: number }
  status: {
    budget: { day: string; used: number; limit: number; retry_at_ms: number; paused_reason: "budget_exhausted" | null }
    pending_events: number
    inflight_events: number
    unknown_events: number
  }
}

export type DeliveryRecord = { work_id: string; target_node: string } & (
  | { state: DispatchState; cwd: string; trigger?: string; title?: string; prompt?: string; reason?: string | null; failure?: FailureKind }
  | { state: StopState }
)
export type HistoryItems = {
  messages: TranscriptMessage; works: WorkRecord; changes: BotChangeRecord; questions: Question; sessions: SessionHandoff
  jobs: ManagerJob; scope_jobs: ScopeJob; owner_jobs: ManagerJob; owner_scope_jobs: ScopeJob; owner_memory_conflicts: ConversationMemoryConflict; checks: CoordinationCheck; deliveries: DeliveryRecord; unanswered: string
  skills: SkillSummary; schedules: ScheduleView; memory: MemoryEntry; memory_conflicts: ConversationMemoryConflict; memory_sync_conflicts: MemorySyncConflict; overwritten: Overwritten
}
export type HistorySection = keyof HistoryItems
export type HistoryCoverage = { next_cursor: string | null; complete: boolean }
export type ConversationHistory = { [S in HistorySection]: HistoryCoverage }
export type HistoryPage = { [S in HistorySection]: { conversation_id: string; history_authority: string; section: S; items: HistoryItems[S][] } & HistoryCoverage }[HistorySection]
