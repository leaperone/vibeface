export type ReplyStatus = "completed" | "stopped" | "interrupted" | "failed"
/** A file on a user's message: hex SHA-256, byte size and its cleaned file name. */
export type Attachment = { hash: string; size: number; name: string }
export type TranscriptMessage = {
  message_id: string
  role: string
  text: string
  event_hash: string
  created_at_ms: number
  in_reply_to?: string
  status?: ReplyStatus
  attachments?: Attachment[]
  /** On a user message: the question it answers. */
  answers?: string
  /** Its place in the conversation's shared order. */
  rank?: string
  /** On a `bot` message: the other bot that wrote it. */
  sender?: { conversation_id: string; name: string }
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
