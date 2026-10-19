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
export type ConversationSummary = { conversation_id: string; title: string }
export type HumanQuestion = { id: string; header: string; question: string; options?: { label: string; description?: string }[]; is_secret?: boolean }
export type HumanRequest = { request_id: string | number; kind: string; title?: string; text: string; choices?: { label: string; response: unknown }[]; questions?: HumanQuestion[] }
export const agentTaskStates = ["pending_delivery", "queued", "claimed", "accepted", "running", "cancelling", "completed", "failed", "cancelled", "interrupted", "unknown"] as const
export type AgentTaskState = typeof agentTaskStates[number]
export const nativeSessionStates = ["running", "waiting", "interrupting", "completed", "failed", "interrupted", "unknown"] as const
export type NativeSessionState = typeof nativeSessionStates[number]
export type AgentTaskRecord = {
  task_ref: string
  source_node: string
  target_node: string
  origin_trigger: string
  state: AgentTaskState
  text: string
  detail: string
}
export type NativeSessionRecord = {
  work_id: string
  task_ref: string | null
  target_node: string
  thread_id: string
  turn_id: string | null
  state: NativeSessionState
  freshness: "observed" | "cached"
  can_control: boolean
  detail: string
  pending_requests: HumanRequest[]
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
export type NotificationRecord = { notification_id: string; kind: string; text: string; conversation_id: string }
export type MissingContext = { events: string[]; blobs: unknown[]; conflicts: string[] }
export type ConversationPayload = {
  conversation: ConversationSummary | null
  messages: TranscriptMessage[]
  heads: string[]
  missing: MissingContext
  agent_tasks: AgentTaskRecord[]
  native_sessions: NativeSessionRecord[]
  observation_targets: string[]
  scheduler: SchedulerSnapshot | null
}
export type WorkspaceView = "chat" | "settings"
export type { PendingMessage }

export type BotProject = { target_node: string; cwd: string }
export type CreateBotRequest = { bot_id: string; conversation_id: string; name: string; instructions: string; project?: BotProject }
export type BotRecord = CreateBotRequest & { event_hash: string }

export type PendingBot = CreateBotRequest & {
  submission: { state: "unconfirmed" | "rejected"; entry_node: string }
}
