import type { PendingMessage, TranscriptMessage } from "../chat/types"

export type NodeInfo = {
  peer_id: string
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
export type WorkState = "accepted" | "starting" | "running" | "waiting_approval" | "interrupting" | "finished" | "unverified"
export type WorkOutcome = "completed" | "failed" | "interrupted" | "rejected" | "unverified"
export type WorkRecord = {
  work_id: string
  trigger: string
  target_node: string
  cwd: string
  prompt: string
  state: WorkState | null
  summary: string
  outcome: WorkOutcome | null
  result_text: string
  failure_reason: string | null
  open_approvals: WorkApproval[]
  answered_approvals: WorkAnswer[]
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
/** A started process or work whose outcome this Node cannot prove; nothing is resent. */
export type PendingCheck = {
  check_id: string
  kind: "manager_process" | "work"
  detail: string
  process_groups: number[]
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
export type { PendingMessage }

export type BotProject = { target_node: string; cwd: string }
export type CreateBotRequest = { bot_id: string; conversation_id: string; name: string; instructions: string; project?: BotProject }
export type BotRecord = CreateBotRequest & { event_hash: string }

export type PendingBot = CreateBotRequest & {
  submission: { state: "unconfirmed" | "rejected"; entry_node: string }
}
