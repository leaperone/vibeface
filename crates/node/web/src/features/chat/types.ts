export type ReplyStatus = "completed" | "stopped" | "interrupted" | "failed"
export type TranscriptMessage = {
  message_id: string
  role: string
  text: string
  event_hash: string
  created_at_ms: number
  in_reply_to?: string
  status?: ReplyStatus
  blobs?: { hash: string; size: number }[]
}
export type PendingMessage = {
  message_id: string
  conversation_id: string
  text: string
  entry_node: string
  report_results?: boolean
  rejected?: { entry_node: string }
}
export type MessageRequest = Pick<PendingMessage, "conversation_id" | "message_id" | "text" | "report_results">
