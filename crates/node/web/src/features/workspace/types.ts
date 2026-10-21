import type { PendingMessage, TranscriptMessage } from "../chat/types"

export type NodeInfo = {
  device_id: string
  name: string
  local: boolean
  online: boolean
  revoked: boolean
  capabilities?: { manager: string | null; relay: boolean; roots: string[]; codex_connected?: boolean }
}
export type ConversationSummary = { conversation_id: string; title: string; owner: string | null; owner_epoch: number }
/** Where a work stands in the signed DAG. */
export type WorkState = "accepted" | "running" | "interrupting" | "finished" | "unverified"
/** A dispatch this device still carries: the target has not taken it, or refused it. Not in the DAG yet. */
export type DispatchState = "dispatch_waiting" | "dispatch_rejected"
/** A stop this device still carries: its device has not confirmed it, or the deadline passed. */
export type StopState = "stop_waiting" | "stop_timed_out"
/** `abandoned`: the device was removed before it reported; the work did not run. */
export type WorkOutcome = "completed" | "failed" | "interrupted" | "rejected" | "unverified" | "abandoned"
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
  /** `codex`: a Codex turn its device watches; `task`: a task run by its device's coordination Agent. */
  kind?: "codex" | "task"
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
  error?: string
  pending_check?: boolean
}
export type SchedulerSnapshot = {
  jobs: ManagerJob[]
  provider?: string
  model?: string
  capabilities?: { streaming: boolean; cancel: boolean; steer: boolean }
}
/** An attachment by content: hex SHA-256 and byte size. */
export type BlobRef = { hash: string; size: number }
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
  questions: Question[]
  notifications: NotificationRecord[]
  /** This device's dispatch and stop requests, as `conversation` returns them. */
  deliveries: unknown[]
  checks: CoordinationCheck[]
  jobs: ManagerJob[]
}
/**
 * What only the person can settle; nothing is resent.
 * `work`: a work waiting for the person's confirmation, answered completed or interrupted. `process`: a coordination process the person confirms stopped.
 */
export type PendingCheck =
  | { kind: "work"; check_id: string; detail: string; work_id: string; target_node: string; conversation_id: string; reason: ConfirmReason }
  | { kind: "process"; check_id: string; detail: string }
export type NotificationRecord = { notification_id: string; kind: string; text: string; conversation_id: string }
export type FieldEdit = { before: string; after: string }
export type AssistantSkill = { name: string; description: string; body: string }
/** Runs at `first_run_at_ms` and every `every_minutes` after it. */
export type AssistantRoutine = { name: string; prompt: string; first_run_at_ms: number; every_minutes: number; enabled: boolean }
/** `before` null is a new item, `after` null a removed one (docs/design/assistant-config.md section 2). */
export type AssistantChange =
  | { kind: "profile"; name?: FieldEdit; instructions?: FieldEdit }
  | { kind: "skill"; skill_id: string; before: AssistantSkill | null; after: AssistantSkill | null }
  | { kind: "routine"; routine_id: string; before: AssistantRoutine | null; after: AssistantRoutine | null }
/** A change applied from the person's message `cause`. */
export type AssistantChangeRecord = { change_id: string; cause: string; event_hash: string; created_at_ms: number; change: AssistantChange }
export type SkillSummary = { skill_id: string; name: string; description: string }
export type RoutineView = AssistantRoutine & { routine_id: string; next_run_at_ms: number | null }
/** This assistant's routines and the skill library every assistant shares. */
export type AssistantView = { skills: SkillSummary[]; routines: RoutineView[] }
export type ConversationPayload = {
  conversation: ConversationSummary | null
  messages: TranscriptMessage[]
  works: WorkRecord[]
  scheduler: SchedulerSnapshot | null
  checks: CoordinationCheck[]
  local_node: string
  /** Oldest first. */
  changes: AssistantChangeRecord[]
  assistant: AssistantView
  /** This chat's questions, answered ones included. */
  questions: Question[]
  sessions: SessionHandoff[]
}
/** "chat" while a conversation route is on screen; reading it then acknowledges its notifications. */
export type WorkspaceView = "chat" | "other"
/** One version of an event, as a Node keeps it when two copies of a device wrote the same place. */
export type ConflictVersion = { hash: string; scope_id: string; created_at_ms: number; body: Record<string, unknown> }
export type LogConflict = { author: string; seq: number; kept: ConflictVersion | null; other: ConflictVersion }
export type WriteState = "aligning" | "read_only" | "writable" | "forked"
export type WriteStatus = { state: WriteState; conflicts: LogConflict[] }
export type { PendingMessage }

export type BotProject = { target_node: string; cwd: string }
export type CreateBotRequest = { bot_id: string; conversation_id: string; name: string; instructions: string; project?: BotProject }
/** An assistant conversation's newest message, cut short by the Node for the conversation list. */
export type LastMessage = { text: string; role: string; created_at_ms: number }
export type BotRecord = CreateBotRequest & { event_hash: string; last_message: LastMessage | null }

export type PendingBot = CreateBotRequest & {
  submission: { state: "unconfirmed" | "rejected"; entry_node: string }
}

export type Severity = "info" | "attention" | "urgent"
/** The device, project and session a question is about. `agent` is "codex" or "claude". */
export type QuestionSubject = { node: string; agent: string; session_id: string; cwd: string; summary: string; request_id?: string }
/** `conversation_id` only on a session question: choosing it hands the session to that assistant. */
export type QuestionChoice = { label: string; conversation_id?: string }
export type QuestionAnswer = { message_id: string; text: string; author: string; created_at_ms: number }
/**
 * Something only the person can settle, asked in an assistant's chat. `conversation_id` null: a session no assistant
 * started, shown in 待处理 only. Text answers are always allowed; a choice answers with its label.
 */
export type Question = {
  question_id: string
  conversation_id: string | null
  assistant: { bot_id: string; name: string } | null
  severity: Severity
  text: string
  choices: QuestionChoice[]
  about: QuestionSubject
  author: string
  created_at_ms: number
  answer: QuestionAnswer | null
}
/** `already_answered`: another answer stood first; `text` is the one that stands. */
export type AnswerResult = { status: "answered" | "already_answered"; text: string }
/** An answer this browser sent whose outcome it has not seen; trying again sends the same signed request. */
export type PendingAnswer = { question_id: string; text: string }
/** A session handed to this assistant, shown where it happened. */
export type SessionHandoff = { node: string; agent: string; session_id: string; cwd: string; summary: string; question_id: string; conversation_id: string; created_at_ms: number }
/** Where questions go besides the web: "both" also sends them to Grok Bot. */
export type Outlet = "record" | "both"
export type TriageSettings = {
  default_outlet: Outlet
  projects: { prefix: string; outlet: Outlet }[]
  webhook_url: string
  scan_codex_sessions: boolean
  webhook_secret_set: boolean
  decision_url: string
  device_id: string
  decision_tokens: { token_id: string; label: string; created_ms: number }[]
  outbox: { pending: number; delivered: number; dead: number }
}
