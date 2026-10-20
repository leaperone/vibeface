import type { PendingMessage, TranscriptMessage } from "../chat/types"

export type NodeInfo = {
  device_id: string
  name: string
  local: boolean
  online: boolean
  revoked: boolean
  capabilities?: { manager: string | null; relay: boolean; roots: string[] }
}
export type ConversationSummary = { conversation_id: string; title: string; owner: string | null; owner_epoch: number }
/** `waiting_for_device` and `rejected` are dispatches another device has not taken. */
export type WorkState = "accepted" | "starting" | "running" | "interrupting" | "finished" | "unverified" | "waiting_for_device" | "rejected"
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
  state: WorkState | null
  summary: string
  outcome: WorkOutcome | null
  result_text: string
  failure_reason: string | null
  /** A stop sent while the device was unreachable: still retrying, or past its deadline. */
  stop?: { waiting_for_device: boolean; timed_out: boolean }
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
/** A started process or work whose outcome this Node cannot prove; nothing is resent. */
export type PendingCheck = {
  check_id: string
  kind: "manager_process" | "work"
  detail: string
  process_groups: number[]
  /** Set on a task check; the user's choice settles that task. */
  work_id?: string
  target_node?: string
}
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
  pending_checks: PendingCheck[]
  local_node: string
  /** Oldest first. */
  changes: AssistantChangeRecord[]
  assistant: AssistantView
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
export type BotRecord = CreateBotRequest & { event_hash: string }

export type PendingBot = CreateBotRequest & {
  submission: { state: "unconfirmed" | "rejected"; entry_node: string }
}

export type Severity = "info" | "attention" | "urgent"
/** A triaged session that needs the person; `choices` turns it into a decision with fixed answers. */
export type Alert = {
  alert_id: string
  matter: string
  agent: string
  session_id: string
  thread_id?: string
  request_id?: string
  choices: { label: string }[]
  turn_id?: string
  cwd: string
  severity: Severity
  summary: string
  count: number
  node: string
  created_at_ms: number
  resolution: "decided" | "dismissed" | null
}
/** `unknown`: the session may have it; sending again with the same decision_id only reads back. */
export type DecisionResult = { decision_id: string; status: "delivered" | "unknown" | "rejected"; reason?: string }
export type Outlet = "record" | "webhook" | "both" | "off"
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
