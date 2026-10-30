import type { ProviderId } from "../lib/node.ts"
import type { PendingMessage, TranscriptMessage } from "../chat/types.ts"
import type { FailureKind } from "../lib/errors.ts"
import type { Avatar } from "../avatar/face.ts"

export type NodeInfo = {
  device_id: string
  name: string
  local: boolean
  online: boolean
  revoked: boolean
  capabilities?: { manager: string | null; manager_state: BotReadiness | null; relay: boolean; roots: string[]; codex_connected?: boolean }
}
/** `ready` only when the bot's switch is on and its provider works; `starting` while it comes up; anything else needs the user. */
export type BotReadiness = "ready" | "starting" | "needs_setup"
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
export type WorkRecord = {
  work_id: string
  trigger: string
  target_node: string
  cwd: string
  /** A few words for the user; `prompt` is what the agent was asked to do. */
  title: string
  prompt: string
  /** A dispatch delivery shows as a work row until its target answers. */
  state: WorkState | DispatchState
  summary: string
  outcome: WorkOutcome | null
  result_text: string
  failure_reason: string | null
  /** Why it failed, as a kind; `failure_reason` keeps the original text. */
  failure?: FailureKind
  /** A provider: a turn on it that its device watches; `task`: a task run by its device's coordination Agent. */
  kind?: ProviderId | "task"
  /** The device that signed the dispatch; absent in snapshots from older Nodes. */
  dispatcher?: string
  /** The caller's idempotency key; absent in snapshots from older Nodes. */
  call_ref?: string
  /** A follow-up work continues the prior work's provider session. */
  continues?: string
  /** The causal parent work, if this was dispatched as a child. */
  parent?: string
  /** The existing provider thread that received this dispatch, when applicable. */
  thread?: string
  /** The durable phase from the latest WorkProgress event. */
  phase?: WorkPhase
  /** The Codex thread and turn running it; works steered into one turn share both. */
  thread_id?: string
  turn_id?: string
  /** A stop sent while the device was unreachable: still retrying, or past its deadline. */
  stop?: StopState
}
export type ManagerJob = {
  trigger: string
  state: string
  message_id?: string
  reply_message_id?: string
  /** A failed or interrupted reply's cause; `detail` is the original text, for 技术详情. */
  failure?: { kind: FailureKind; detail: string }
  pending_check?: boolean
  /** How the reply a steered message joined ended. */
  outcome?: SteerOutcome
  /** The owner device that reported it; absent for this device's own jobs. */
  source_node?: string
}
export type SteerOutcome = 'answered' | 'unfinished' | 'unknown'
export type SchedulerSnapshot = {
  jobs: ManagerJob[]
  /** Messages a former scheduling device left unanswered; only listed on the device that schedules now. */
  unanswered: string[]
  provider?: string
  model?: string
  capabilities?: { streaming: boolean; cancel: boolean; steer: boolean }
  /** Set when another device schedules this conversation; unreachable leaves its replies' state unknown. */
  remote?: { owner: string; reachable: boolean }
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
/** What one change touched, pushed by the Node so the page merges it without a reload. */
export type ConversationRecords = {
  conversation_id: string
  messages: TranscriptMessage[]
  works: WorkRecord[]
  last_message: LastMessage | null
  busy: Busy
  questions: Question[]
  notifications: NotificationRecord[]
  /** This device's dispatch and stop requests, as `conversation` returns them. */
  deliveries: unknown[]
  checks: CoordinationCheck[]
  jobs: ManagerJob[]
  unanswered: string[]
  sent: SentBotMessage[]
}
/**
 * What only the person can settle; nothing is resent.
 * `work`: a work waiting for the person's confirmation, answered completed or interrupted. `device`: several works one offline
 * device or one disconnected Codex cannot report, settled one by one on a single card. `process`: a coordination process the person confirms stopped.
 */
export type PendingCheck =
  | { kind: "work"; check_id: string; detail: string; work_id: string; target_node: string; conversation_id: string; title: string; reason: ConfirmReason }
  | { kind: "device"; check_id: string; detail: string; target_node: string; reason: "offline" | "disconnected"; works: Extract<PendingCheck, { kind: "work" }>[] }
  | { kind: "process"; check_id: string; detail: string }
export type NotificationRecord = { notification_id: string; kind: string; text: string; conversation_id: string }
export type FieldEdit = { before: string; after: string }
/** A bot's model on one provider; empty follows the device's default. */
export type ModelEdit = FieldEdit & { provider: ProviderId }
/** A provider without an entry runs the device's default model. */
export type BotModels = Partial<Record<ProviderId, string>>
export type BotSkill = { name: string; description: string; body: string }
/** Runs at `first_run_at_ms`, then every `every_minutes` when set; without it, once. `depth` > 0 is a reminder the bot set itself. */
export type BotSchedule = { name: string; prompt: string; first_run_at_ms: number; every_minutes?: number; enabled: boolean; depth?: number }
type MemoryEntry = { memory_id: string; text: string }
/** null: every shared skill, later ones included. */
export type SkillSet = string[] | null
type SkillsEdit = { before: SkillSet; after: SkillSet }
/** null: the face its id gives. */
export type AvatarEdit = { before: Avatar | null; after: Avatar | null }
/** `before` null is a new item, `after` null a removed one (docs/design/bot-config.md section 2). */
export type BotChange =
  | { kind: "profile"; name?: FieldEdit; instructions?: FieldEdit; model?: ModelEdit; skills?: SkillsEdit; avatar?: AvatarEdit }
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
  | { field: "skills"; value: SkillSet }
  | { field: "avatar"; value: Avatar | null })
type SkillSummary = { skill_id: string; name: string; description: string }
export type ScheduleView = BotSchedule & { schedule_id: string; next_run_at_ms: number | null }
export type ScheduleAction = "pause" | "resume" | "delete"
/** This bot's schedules still ahead, its notes, and the skill library every bot shares. */
export type BotSettings = { skills: SkillSummary[]; schedules: ScheduleView[]; memory: MemoryEntry[] }
export type ConversationPayload = {
  conversation: ConversationSummary | null
  messages: TranscriptMessage[]
  works: WorkRecord[]
  scheduler: SchedulerSnapshot | null
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
}
export type SentBotMessage = { message_id: string; to: string; text: string; created_at_ms: number }
/** "chat" while a conversation route is on screen; reading it then acknowledges its notifications. */
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
export type BotRecord = CreateBotRequest & { models: BotModels; skills: SkillSet; avatar: Avatar | null; avatar_version: string | null; event_hash: string; last_message: LastMessage | null; busy: Busy }

export type OverviewBot = Pick<BotRecord, "bot_id" | "conversation_id" | "name">
export type OverviewNode = Pick<NodeInfo, "device_id" | "name" | "online" | "revoked" | "local">
export type OverviewConsistency = "local_snapshot" | "mixed_remote" | "partial"
export type OverviewTaskState = WorkState | DispatchState | StopState | "accepted_waiting_sync" | "unknown"
export type OverviewDeliveryState = DispatchState | "dispatch_accepted" | StopState
export type OverviewTaskKind = "work" | "check" | "dispatch"
export type OverviewNavigation = { bot_id: string; conversation_id: string; work_id?: string }

type OverviewTaskCommon = {
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
  kind: "question"
  question_id: string
  conversation_id: string
  bot: OverviewBot | null
  text: string
  severity: Severity
  choices: { label: string }[]
  request_id: string | null
  work_id: string | null
  about: QuestionSubject | null
  created_at_ms: number
  state: "unanswered"
  navigation: { bot_id: string; conversation_id: string } | null
}

export type OverviewJob = {
  job_id: string
  conversation_id: string | null
  trigger: string
  state: string
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

export type PendingBot = CreateBotRequest & {
  submission: { state: "unconfirmed" | "rejected"; entry_node: string }
}

export type Severity = "info" | "attention" | "urgent"
/** The device, project and session a question is about. `agent` is null when the question is about the device's own settings. */
export type QuestionSubject = { node: string; agent: ProviderId | null; session_id: string; cwd: string; summary: string; request_id?: string }
type QuestionChoice = { label: string }
type QuestionAnswer = { message_id: string; text: string; author: string; created_at_ms: number }
/** Why the device that asked closed a question unanswered: the App Server request it held is gone. */
export type CloseReason = "request_lost"
/** Something only the person can settle, asked in a bot's chat. Text answers are always allowed; a choice answers with its label. */
export type Question = {
  question_id: string
  conversation_id: string
  bot: { bot_id: string; name: string } | null
  severity: Severity
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
export type SessionHandoff = { node: string; agent: ProviderId; session_id: string; cwd: string; summary: string; muted: boolean; created_at_ms: number }
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
}
