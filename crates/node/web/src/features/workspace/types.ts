import type { PendingMessage, TranscriptMessage } from "../chat/types"

export type NodeInfo = {
  device_id: string
  name: string
  local: boolean
  online: boolean
  revoked: boolean
  roles: string[]
  capabilities?: { manager: string | null; roots: string[] }
}
export type ConversationSummary = { conversation_id: string; title: string; owner: string | null }
export type HumanQuestion = { id: string; header: string; question: string; options?: { label: string; description?: string }[]; is_secret?: boolean }
/** An open executor request. `form` exists only on the device that runs the work. */
export type WorkApproval = { request_id: string; kind: string; summary: string; answering?: boolean; form?: { command?: string; cwd?: string; reason?: string; files?: string[]; choices?: { label: string; response: unknown }[]; questions?: HumanQuestion[] } }
/** An answered request stays as one line in the chat; the decision comes from the shared `ApprovalAnswered` event. */
export type WorkAnswer = { request_id: string; kind: string; summary: string; decision: string }
/** `waiting_for_device` and `rejected` are dispatches another device has not taken. */
export type WorkState = "accepted" | "starting" | "running" | "waiting_approval" | "interrupting" | "finished" | "unverified" | "waiting_for_device" | "rejected"
/** `abandoned`: the device was removed before it reported; the work did not run. */
export type WorkOutcome = "completed" | "failed" | "interrupted" | "rejected" | "unverified" | "abandoned"
export type WorkRecord = {
  work_id: string
  trigger: string
  target_node: string
  cwd: string
  /** A few words for the user; `prompt` is the executor's instructions. */
  title: string
  prompt: string
  state: WorkState | null
  summary: string
  outcome: WorkOutcome | null
  result_text: string
  failure_reason: string | null
  open_approvals: WorkApproval[]
  answered_approvals: WorkAnswer[]
  /** A stop sent while the device was unreachable: still retrying, or past its deadline. */
  stop?: { waiting_for_device: boolean; timed_out: boolean }
}
export type ManagerJob = {
  trigger: string
  state: string
  kind: string
  sequence: number
  message_id?: string
  reply_message_id?: string
  read_only?: boolean
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
export type AutonomyMode = "off" | "report" | "continue"
export type ConversationAutonomy = { mode: AutonomyMode; version: number }
export type ConversationPayload = {
  conversation: ConversationSummary | null
  messages: TranscriptMessage[]
  autonomy: ConversationAutonomy
  works: WorkRecord[]
  scheduler: SchedulerSnapshot | null
  pending_checks: PendingCheck[]
  local_node: string
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
