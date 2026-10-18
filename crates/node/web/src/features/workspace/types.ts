import type { PendingMessage, TranscriptMessage } from "../chat/types"
import type { AuthorizedRequest, ControlFacts } from "../../lib/node"
import type { TrustedRemoteExecution } from "../../lib/remote-execution"
import type { ProjectExecutionProof, VerifiedProjectWork } from "../../lib/execution-work"

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
  task_creation: Record<string, unknown>
  authority_public_key: string
  binding: Record<string, unknown> | null
}
type SessionFields = {
  executor: string
  target_node: string
  state: string
  cwd: string
  session_id: string
  revision: number
  native_session_id?: string
}
export type TaskSessionRecord = SessionFields & { task_id: string; binding: Record<string, unknown>; task_creation: Record<string, unknown>; authority_public_key: string }
export type ConversationSessionRecord = SessionFields & { task_id: null; binding: null; task_creation: null; authority_public_key: null }
export type SessionRecord = TaskSessionRecord | ConversationSessionRecord
export type OperationRecord = {
  state: string
  request: OperationRequest
  raw: unknown
  execution_source?: Record<string, unknown>
  proof?: ProjectExecutionProof
  verified?: VerifiedProjectWork
  proofState?: 'unverified_target'
}
export type NotificationRecord = { notification_id: string; kind: string; text: string; conversation_id: string }
export type MissingContext = { events: string[]; blobs: unknown[]; conflicts: string[] }
export type ConversationPayload = {
  conversation: ConversationSummary | null
  messages: TranscriptMessage[]
  heads: string[]
  missing: MissingContext
  executions: ExecutionActivityRecord[]
  scheduler: SchedulerSnapshot | null
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

type OperationFields = {
  operation_id: string
  space_id: string
  conversation_id: string
  target_node: string
  expected_revision: number
  basis_heads: string[]
  resources: string[]
  manager_authority?: Record<string, unknown> | null
}
export type TaskOperationRequest = OperationFields & { task_id: string; binding: Record<string, unknown>; session_id: string; action: OperateAction }
export type ConversationOperationRequest = OperationFields & { task_id: null; binding?: null; session_id: string; action: OperateAction }
export type CommandOperationRequest = OperationFields & { task_id: null; binding?: null; session_id: null; action: { type: 'command'; command: string; cwd: string; timeout_secs: number } }
export type OperationRequest = TaskOperationRequest | ConversationOperationRequest | CommandOperationRequest
export type PendingOperation = Pick<OperationRequest, "space_id" | "operation_id" | "target_node" | "session_id" | "conversation_id"> & { request: OperationRequest; attempt: AuthorizedRequest | null; remote?: TrustedRemoteExecution; execution_session_receipt?: Record<string, unknown> }
export type PendingConversation = ConversationSummary & { attempt: AuthorizedRequest | null }
export type TaskCreationRequest = { conversation_id: string; task_id: string; title: string; goal: string; acceptance: string; auto_followup: boolean }
export type PendingTask = TaskCreationRequest & { attempts: AuthorizedRequest[] }
export type PendingBinding = {
  request: Omit<TaskOperationRequest, "binding"> & { action: Extract<OperateAction, { type: "open" }> }
  task: TaskRecord
  attempt: AuthorizedRequest | null
  remote?: TrustedRemoteExecution
}
export type PendingRemoteResolution = {
  request: OperationRequest
  remote: TrustedRemoteExecution
  observation: Record<string, unknown>
  body: Record<string, unknown>
  context: Record<string, unknown>
  attempt: AuthorizedRequest | null
}
export type WorkspaceView = "chat" | "settings"
export type { PendingMessage }

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
  attempt_id?: string | null
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

export type ExecutionControlAction =
  | { type: "interrupt" }
  | { type: "steer"; text: string }
  | { type: "respond_request"; request_id: string | number; response: unknown }
export type ExecutionControlRequest = {
  basis_heads: string[]
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

export type BotProject = { target_node: string; cwd: string }
export type CreateBotRequest = { bot_id: string; conversation_id: string; name: string; instructions: string; project?: BotProject }
export type BotRecord = CreateBotRequest & { event_hash: string }

export type PendingBot = CreateBotRequest & {
  submission: { state: "unconfirmed" | "rejected"; entry_node: string }
  attempt: AuthorizedRequest | null
}

export type PendingExecutionControl = { operation_id: string; target_node: string; session_id: string | null; conversation_id: string; execution_control: ExecutionControlRequest; attempt: AuthorizedRequest | null; control: ControlFacts | null; remote?: TrustedRemoteExecution; request?: OperationRequest }
