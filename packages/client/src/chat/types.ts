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
  /** On a user message: the question it answers. */
  answers?: string
  /** Its place in the conversation's shared order. */
  rank?: string
}
export type PendingMessage = {
  message_id: string
  conversation_id: string
  text: string
  report_results?: boolean
  /** The entry's refusal, kept so the person can drop it; a lost answer leaves this unset. */
  rejected?: string
}
export type MessageRequest = Pick<PendingMessage, "conversation_id" | "message_id" | "text" | "report_results">
