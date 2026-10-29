import type { TranscriptMessage } from '@vibeface/client/chat/types.ts'
import type { AnswerResult, BotReadiness, BotRecord, Question, WorkRecord } from '@vibeface/client/workspace/types.ts'

export type { Question }

/** How this phone reaches its network right now. `entry`: the device it goes through, as the person named it; `direct`: straight to it on the LAN. */
export type Connection = { state: 'connecting' | 'online' | 'syncing' | 'offline'; entry: string; direct: boolean }

/**
 * Where this phone stands with the network. `joining`: the invitation went out and an admin must allow it;
 * `code` is the safety code both screens show, null until the admin starts the comparison.
 */
export type Session =
  | { phase: 'unjoined' }
  | { phase: 'joining'; code: string | null }
  | { phase: 'ready' }

/** A transcript message, or one this phone shows before the network holds it. `detail`: a failed reply's original error from the device. */
export type ChatMessage = TranscriptMessage & { state?: 'sending' | 'streaming' | 'interrupted'; detail?: string }

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

/** `bot`: how the device's bot stands right now, `null` with none; removing it stops that bot. `online`: known for computers only. */
export type Member = { device_id: string; name: string; kind: 'node' | 'client'; admin: boolean; online: boolean | undefined; current: boolean; entry: boolean; bot: BotReadiness | null }

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
  /** Takes this phone out of the network; coming back takes a join link. */
  leave(): Promise<void>
}

export type Workspace = WorkspaceSnapshot & { actions: WorkspaceActions }
