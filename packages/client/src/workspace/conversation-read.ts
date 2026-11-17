import type { TranscriptMessage } from '../chat/types.ts'
import type { NotificationRecord } from './types.ts'
import { unreadReply } from '../chat/timeline.ts'
import { parseConversation } from './model.ts'
import { mergeConversationSnapshot } from './history.ts'
import type { ConversationPayload } from './types.ts'

/** Owns the active read, independently of the slower workspace metadata refresh. */
export class ConversationReadOwner {
  private sequence = 0
  private observed = new Map<string, string>()
  private read = false
  invalidate() { this.sequence += 1; this.clearConfirmation() }
  clearConfirmation() { this.observed.clear(); this.read = false }
  start(current: () => boolean) {
    const sequence = ++this.sequence
    return { current: () => sequence === this.sequence && current() }
  }
  confirmed() { return this.read }
  observedMessage(message: TranscriptMessage) { return this.read && this.observed.get(message.message_id) === message.event_hash }
  confirm(messages: readonly TranscriptMessage[], replace = false) {
    if (replace) this.observed.clear()
    this.read = true
    for (const message of messages) this.observed.set(message.message_id, message.event_hash)
  }
  /** A visible stable ID acknowledges only the exact event this connection actually returned. */
  notificationsForMessages(input: { messageIds: readonly string[]; messages: readonly TranscriptMessage[]; notifications: readonly NotificationRecord[]; conversationId: string }): NotificationRecord[] {
    if (!this.read) return []
    const visible = new Set(input.messageIds)
    const hashes = new Set(input.messages.filter((message) => visible.has(message.message_id) && this.observedMessage(message)).map((message) => message.event_hash))
    return input.notifications.filter((item) => item.conversation_id === input.conversationId && unreadReply(item) && hashes.has(item.notification_id))
  }
}

/** Shared by the hook and behavior tests: a cancelled read has no state, error or permission effect. */
export async function readConversationSnapshot(input: {
  conversationId: string
  owner: ConversationReadOwner
  current(): boolean
  read(): Promise<unknown>
  shown(): ConversationPayload | null
  loading(value: boolean): void
  failed(caught: unknown): void
  authorityChanged(): void
  apply(value: ConversationPayload): Promise<ConversationPayload>
}): Promise<ConversationPayload | null> {
  const baseline = input.shown(), request = input.owner.start(input.current)
  input.loading(true)
  try {
    const next = parseConversation(await input.read(), input.conversationId)
    if (!request.current()) return null
    const shown = input.shown()
    if (shown && shown.history_authority !== next.history_authority) {
      input.owner.clearConfirmation()
      input.authorityChanged()
    }
    input.owner.confirm(next.messages)
    return await input.apply(mergeConversationSnapshot(shown, next, baseline))
  } catch (caught) {
    if (!request.current()) return null
    input.failed(caught)
    throw caught
  } finally { if (request.current()) input.loading(false) }
}

/** Visibility is rechecked between signed acknowledgments, including after a route or foreground change. */
export async function markVisibleMessagesRead(input: {
  owner: ConversationReadOwner
  messageIds: readonly string[]
  messages: readonly TranscriptMessage[]
  notifications: readonly NotificationRecord[]
  conversationId: string
  current(): boolean
  acknowledge(notification: NotificationRecord): Promise<void>
}): Promise<void> {
  if (!input.current()) return
  const selected = input.owner.notificationsForMessages(input)
  for (const notification of selected) {
    if (!input.current() || !input.owner.confirmed()) return
    await input.acknowledge(notification)
  }
}
