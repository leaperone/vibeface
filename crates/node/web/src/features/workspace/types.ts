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
  /** The Codex thread running it; a decision card names the same thread. */
  thread_id?: string
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
/**
 * What only the person can settle; nothing is resent.
 * `work`: an unverified work, answered completed or interrupted. `process`: a coordination process the person confirms stopped.
 */
export type PendingCheck =
  | { kind: "work"; check_id: string; detail: string; work_id: string; target_node: string }
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
