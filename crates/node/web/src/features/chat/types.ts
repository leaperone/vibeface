export type ConversationScreenProps = {
  conversationId: string | null
  schedulerNodeId: string | null
  missing: boolean
  schedulerOnline: boolean
  managerReady: boolean
}

export type TranscriptMessage = {
  message_id: string
  role: string
  text: string
  event_hash?: string
  created_at_ms?: number
}

export type MissingContext = {
  events?: unknown
  blobs?: unknown
  conflicts?: unknown
}

export type ConversationPayload = {
  messages: TranscriptMessage[]
  heads: string[]
  missing: MissingContext
}

export type PendingMessage = {
  message_id: string
  conversation_id: string
  text: string
  scheduler_node?: string
}

export type MessageRequest = {
  conversation_id: string
  message_id: string
  text: string
  basis_heads: string[]
  scheduler_node: string
}
