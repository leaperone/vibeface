export type NodeInfo = {
  node_id: string
  local?: boolean
  online?: boolean
  roles?: string[]
  capabilities?: {
    manager?: boolean
    executors?: string[]
    roots?: string[]
  }
}

export type ConversationSummary = {
  conversation_id: string
  title?: string
}

export type TaskRecord = {
  task_id: string
  title?: string
  goal?: string
  acceptance?: string
  detail?: string
  status?: string
}

export type SessionRecord = {
  executor?: string
  target_node: string
  state?: string
  cwd?: string
  session_id: string
  revision?: number
  native_session_id?: string
}

export type OperationRecord = {
  state?: string
  request?: {
    operation_id?: string
    target_node?: string
  }
  raw: unknown
}

export type NotificationRecord = {
  notification_id: string
  kind?: string
  text?: string
  conversation_id: string
}

export type ConversationPayload = {
  conversation?: { title?: string }
  heads?: string[]
  missing?: { events?: unknown; blobs?: unknown; conflicts?: unknown }
  tasks?: TaskRecord[]
  sessions?: SessionRecord[]
  operations?: OperationRecord[]
}

export type PendingOperation = {
  operation_id: string
  target_node: string
  session_id?: string
  conversation_id: string
}

export type WorkspaceView = "chat" | "tasks" | "sessions" | "devices" | "notifications"

export type EntryInfo = {
  node_id?: string
  version?: string
}

export type ChatSlotState = {
  conversationId: string | null
  schedulerNodeId: string | null
  missing: boolean
  schedulerOnline: boolean
  managerReady: boolean
}

export type OperateAction = {
  type: string
  text?: string
  turn_id?: string
  executor?: string
  cwd?: string
  request_id?: string
  response?: Record<string, unknown>
}
