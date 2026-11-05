import type { FailureKind } from "../lib/errors.ts"
import type { InputIntent, InputReceipt, InputTarget } from "../workspace/types.ts"
import type { OutboxItem } from "../lib/outbox.ts"

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
  /** On a failed or interrupted reply: why, as a kind; the original text stays on its device. */
  failure?: FailureKind
  attachments?: Attachment[]
  /** On a user message: the question it answers. */
  answers?: string
  /** Its place in the conversation's shared order. */
  rank?: string
  /** On a `bot` message: the other bot that wrote it. */
  sender?: { conversation_id: string; name: string }
  target?: InputTarget
}
export type PendingMessage = {
  message_id: string
  conversation_id: string
  text: string
  report_results?: boolean
  target: InputTarget
  intent: InputIntent
  sequence: number
  expected_revision?: number
  attachments?: Attachment[]
  created_at_ms: number
  delivery: 'saved' | 'uploading' | 'submitting' | 'accepted' | 'unknown'
  receipt?: InputReceipt
  /** The entry's refusal, kept so the person can drop it; a lost answer leaves this unset. */
  rejected?: string
  rejection_code?: OutboxItem['rejection_code']
}
export type MessageRequest = Pick<PendingMessage, "conversation_id" | "message_id" | "text" | "report_results" | "target" | "intent" | "sequence" | "expected_revision" | "attachments">
