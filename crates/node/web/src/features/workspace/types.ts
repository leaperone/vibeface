import type { PendingMessage, TranscriptMessage } from "../chat/types"

export type NodeInfo = {
  node_id: string
  local: boolean
  online: boolean
  revoked: boolean
  roles: string[]
  capabilities?: { manager: string | null; executors: string[]; roots: string[] }
}
export type ConversationSummary = { conversation_id: string; title: string }
export type TaskRecord = {
  task_id: string
  title: string
  goal: string
  acceptance: string
  detail: string
  status: string
  followup_mode?: FollowupMode
  followup_version?: number
  followup_scheduler_node?: string
  blobs?: { hash: string; size: number }[]
  followup_error?: string
  binding_error?: string
  binding?: TaskBinding
}
export type FollowupMode = "off" | "report" | "continue"
export type HumanQuestion = { id: string; header: string; question: string; options?: { label: string; description?: string }[]; is_secret?: boolean }
export type HumanRequest = { request_id: string | number; kind: string; title?: string; text: string; choices?: { label: string; response: unknown }[]; questions?: HumanQuestion[] }
export type TaskBinding = {
  target_node: string
  session_id: string
  operation_id?: string
  state?: string
  capabilities: string[]
  pending_requests: HumanRequest[]
  current_turn?: string
  latest_progress?: string
  revision: number
  freshness?: "local_ledger" | "cached" | "unknown"
  checked_at_ms?: number
}
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
export type SessionRecord = {
  executor: string
  target_node: string
  state: string
  cwd: string
  session_id: string
  revision: number
  native_session_id?: string
  capabilities?: string[]
}
export type OperationRecord = {
  state: string
  request: { operation_id: string; target_node: string; conversation_id: string; session_id: string }
  raw: unknown
}
export type NotificationRecord = { notification_id: string; kind: string; text: string; conversation_id: string }
export type MissingContext = { events: string[]; blobs: unknown[]; conflicts: string[] }
export type ConversationPayload = {
  conversation: ConversationSummary | null
  messages: TranscriptMessage[]
  heads: string[]
  missing: MissingContext
  tasks: TaskRecord[]
  sessions: SessionRecord[]
  operations: OperationRecord[]
  scheduler: SchedulerSnapshot | null
}
export type OperateAction =
  | { type: "open"; executor: string; cwd: string }
  | { type: "read" }
  | { type: "send"; text: string }
  | { type: "steer"; text: string; turn_id: string }
  | { type: "interrupt"; turn_id: string }
  | { type: "respond_request"; request_id: string | number; response: Record<string, unknown> }

export type OperationRequest = {
  operation_id: string
  network_id: string
  conversation_id: string
  task_id: string | null
  target_node: string
  session_id: string
  expected_revision: number
  basis_heads: string[]
  resources: string[]
  action: OperateAction
}
export type TaskControlRequest = {
  operation_id: string
  network_id: string
  conversation_id: string
  task_id: string
  target_node: string
  session_id: string
  expected_operation_id: string
  expected_revision: number
  action: "interrupt" | "steer" | "respond_request"
  text?: string
  request_id?: string | number
  response?: unknown
}
export type PendingOperation = Pick<OperationRequest, "operation_id" | "target_node" | "session_id" | "conversation_id"> & ({ request: OperationRequest; control?: never } | { request?: never; control: TaskControlRequest })
export type PendingConversation = { conversation_id: string; title: string }
export type PendingTask = { conversation_id: string; task_id: string; title: string; goal: string; acceptance: string }
export type WorkspaceView = "chat" | "tasks" | "sessions" | "devices" | "notifications"
export type ChatSlotState = {
  conversationId: string | null
  schedulerNodeId: string | null
  missing: boolean
  schedulerOnline: boolean
  schedulerAllowed: boolean
}
export type { PendingMessage }
