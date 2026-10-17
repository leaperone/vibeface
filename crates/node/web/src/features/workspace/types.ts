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
}
export type SessionRecord = {
  executor: string
  target_node: string
  state: string
  cwd: string
  session_id: string
  revision: number
  native_session_id?: string
}
export type OperationRecord = {
  state: string
  request: { space_id: string; operation_id: string; target_node: string; conversation_id: string; session_id: string }
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
  space_id: string
  conversation_id: string
  task_id: null
  target_node: string
  session_id: string
  expected_revision: number
  basis_heads: string[]
  resources: string[]
  action: OperateAction
}
export type PendingOperation = Pick<OperationRequest, "space_id" | "operation_id" | "target_node" | "session_id" | "conversation_id"> & { request: OperationRequest }
export type PendingConversation = { conversation_id: string; title: string }
export type PendingTask = { conversation_id: string; task_id: string; title: string; goal: string; acceptance: string; auto_followup: boolean }
export type WorkspaceView = "chat" | "tasks" | "sessions" | "devices" | "notifications"
export type ChatSlotState = {
  conversationId: string | null
  schedulerNodeId: string | null
  missing: boolean
  schedulerOnline: boolean
  managerReady: boolean
}
export type { PendingMessage }
