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
/** A dispatch this device still carries: the target has not taken it, refused it, or was removed before saying. Not in the DAG yet. */
type DispatchState = "dispatch_waiting" | "dispatch_rejected" | "dispatch_unverified"
/** A stop this device still carries: its device has not confirmed it, or the deadline passed. */
type StopState = "stop_waiting" | "stop_timed_out"
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
  bot_settings: BotSettings
  /** This chat's questions, answered ones included. */
  questions: Question[]
  sessions: SessionHandoff[]
  /** What this bot wrote to other bots; `to` is the receiver's conversation. */
  sent: SentBotMessage[]
}
export type SentBotMessage = { message_id: string; to: string; text: string; created_at_ms: number }
/** "chat" while a conversation route is on screen; reading it then acknowledges its notifications. */
export type WorkspaceView = "chat" | "other"
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
