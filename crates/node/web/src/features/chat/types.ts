import type { AuthorizedRequest } from "../../lib/node"

export type TranscriptMessage = {
  message_id: string
  role: string
  text: string
  event_hash: string
  created_at_ms: number
}
export type PendingMessage = {
  message_id: string
  conversation_id: string
  text: string
  scheduler_node: string
  entry_node: string
  basis_heads: string[]
  attempt: AuthorizedRequest | null
  rejected?: { entry_node: string }
}
export type MessageRequest = Pick<PendingMessage, "conversation_id" | "message_id" | "text" | "basis_heads" | "scheduler_node">
