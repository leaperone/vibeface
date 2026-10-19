import type { PendingMessage, TranscriptMessage } from "../chat/types"

export type NodeInfo = {
  node_id: string
  name: string
  local: boolean
  online: boolean
  revoked: boolean
  roles: string[]
  capabilities?: { manager: string | null; executors: string[]; roots: string[] }
}
export type ConversationSummary = { conversation_id: string; title: string }
export type FollowupMode = "off" | "report" | "continue"
export type HumanQuestion = { id: string; header: string; question: string; options?: { label: string; description?: string }[]; is_secret?: boolean }
export type HumanRequest = { request_id: string | number; kind: string; title?: string; text: string; choices?: { label: string; response: unknown }[]; questions?: HumanQuestion[] }
export type ManagerJob = {
  trigger: string
  state: string
  kind: string
  sequence: number
  message_id?: string
  target_node?: string
  reply_message_id?: string
  read_only?: boolean
  error?: string
  delivery_error?: string
}
export type SchedulerSnapshot = {
  jobs: ManagerJob[]
  provider?: string
  model?: string
  capabilities?: { streaming: boolean; cancel: boolean; steer: boolean }
  write_frozen?: boolean
}
export type OperationRecord = {
  state: string
  request: { operation_id: string; target_node: string; conversation_id: string; session_id: string | null }
  raw: unknown
}
export type NotificationRecord = { notification_id: string; kind: string; text: string; conversation_id: string }
export type MissingContext = { events: string[]; blobs: unknown[]; conflicts: string[] }
export type ConversationPayload = {
  conversation: ConversationSummary | null
  messages: TranscriptMessage[]
  heads: string[]
  missing: MissingContext
  executions: ExecutionActivityRecord[]
  operations: OperationRecord[]
  scheduler: SchedulerSnapshot | null
}
export type ExecutionControlAction =
  | { type: "interrupt" }
  | { type: "steer"; text: string }
  | { type: "respond_request"; request_id: string | number; response: unknown }
export type ExecutionControlRequest = {
  conversation_id: string
  operation_id: string
  expected_operation_id: string
  expected_revision: number
  action: ExecutionControlAction
}
export type ExecutionActivityRecord = {
  operation_id: string
  target_node: string
  session_id: string | null
  revision: number
  title: string
  state: string
  session_state: string
  capabilities: string[]
  pending_requests: HumanRequest[]
  freshness: "local_ledger" | "cached" | "unknown"
  checked_at_ms?: number
  latest_progress?: string
  detail: string
  blobs: { hash: string; size: number }[]
  source_trigger?: string
  source_message_id?: string
  goal?: { task_id: string; title: string; acceptance: string; status: string; followup_mode: FollowupMode; followup_version: number; followup_scheduler_node?: string }
}
export type PendingOperation = {
  operation_id: string
  target_node: string
  session_id: string | null
  conversation_id: string
  execution_control: ExecutionControlRequest
}
export type WorkspaceView = "chat" | "settings"
export type { PendingMessage }

export type BotProject = { target_node: string; cwd: string }
export type CreateBotRequest = { bot_id: string; conversation_id: string; name: string; instructions: string; project?: BotProject }
export type BotRecord = CreateBotRequest & { event_hash: string }

export type PendingBot = CreateBotRequest & {
  submission: { state: "unconfirmed" | "rejected"; entry_node: string }
}
