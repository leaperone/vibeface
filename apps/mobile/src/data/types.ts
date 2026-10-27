import type { TranscriptMessage } from '@vibeface/client/chat/types.ts'
import type { AnswerResult, BotRecord, Question, WorkRecord } from '@vibeface/client/workspace/types.ts'

export type { Question }

/** How this phone reaches its network right now. `entry`: the device it goes through, as the person named it. */
export type Connection = { state: 'connecting' | 'online' | 'syncing' | 'offline'; entry: string }

/**
 * Where this phone stands with the network. `joining`: the invitation went out and an admin must allow it;
 * `code` is the safety code both screens show, null until the admin starts the comparison.
 */
export type Session =
  | { phase: 'unjoined' }
  | { phase: 'joining'; code: string | null }
  | { phase: 'ready' }

/** A transcript message, or one this phone shows before the network holds it. */
export type ChatMessage = TranscriptMessage & { state?: 'sending' | 'streaming' | 'interrupted' }

/**
 * One bot's chat as the screen draws it. `status`: the live progress word while a reply runs.
 * `stoppable`: the running reply can be cancelled. `blocked`: why sending is off right now, or null when it is allowed.
 */
export type Conversation = {
  messages: ChatMessage[]
  questions: Question[]
  works: WorkRecord[]
  replying: boolean
  status: string | null
  stoppable: boolean
  blocked: string | null
  sending: boolean
}

/** A file picked for the next message. */
export type Attachment = { uri: string; name: string; size: number | null; mimeType: string | null }

/** `bot`: the device runs a bot right now, so removing it stops that bot. */
export type Member = { device_id: string; name: string; kind: 'node' | 'client'; admin: boolean; online: boolean; current: boolean; entry: boolean; bot: boolean }

type WorkspaceSnapshot = {
  session: Session
  connection: Connection
  bots: BotRecord[]
  /** The primary bot's conversation. */
  primary: string | null
  /** Unread notifications and open questions per conversation. */
  unread: Record<string, number>
  /** Every question this network holds, answered ones included. */
  questions: Question[]
  members: Member[]
  /** Loaded chats by conversation; a chat appears once `open` has loaded it. */
  conversations: Record<string, Conversation>
}

export type WorkspaceActions = {
  join(invitation: string, deviceName: string): Promise<void>
  cancelJoin(): void
  /** Loads the chat and keeps it live while on screen; reading it acknowledges its notifications. */
  open(conversationId: string): () => void
  send(conversationId: string, text: string, attachments: Attachment[]): Promise<void>
  stop(conversationId: string): Promise<void>
  answer(question: Question, text: string): Promise<AnswerResult>
  disconnect(): void
}

export type Workspace = WorkspaceSnapshot & { actions: WorkspaceActions }
