import type { AuthorizedRequest } from "../../lib/node"

export type TranscriptMessage = {
  message_id: string
  role: string
  text: string
  event_hash: string
  created_at_ms: number
  blobs?: { hash: string; size: number }[]
}
export type PendingMessage = {
  message_id: string
  conversation_id: string
  text: string
  scheduler_node: string | null
  entry_node: string
  basis_heads: string[]
  attempt: AuthorizedRequest | null
  report_results?: boolean
  selected_route?: string
  rejected?: { entry_node: string }
}
export type MessageRequest = Pick<PendingMessage, "conversation_id" | "message_id" | "text" | "basis_heads" | "scheduler_node" | "entry_node" | "report_results">
