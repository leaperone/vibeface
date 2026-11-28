import { createConversationCache, type CachedConversation } from '@vibeface/client/workspace/conversation-cache.ts'
import { chatTimeline } from '@vibeface/client/chat/timeline.ts'
import { foldAnswers } from '@vibeface/client/workspace/questions.ts'
import type { PendingMessage } from '@vibeface/client/chat/types.ts'
import type { ConversationPayload } from '@vibeface/client/workspace/types.ts'
import type { Conversation } from './types'
import { mobilePlatform } from '@/platform'

export const localConversations = createConversationCache({ storage: mobilePlatform.storage })

// Snapshots keep their payload object until saved again, so each is projected once per save and set of its own pending inputs.
const projections = new WeakMap<ConversationPayload, { conversationId: string; savedAt: number; pending: readonly PendingMessage[]; conversation: Conversation }>()

export function cachedConversation(record: CachedConversation, pending: readonly PendingMessage[] = []): Conversation {
  const { payload } = record
  const own = pending.filter((item) => item.conversation_id === record.conversationId && item.target.kind === 'bot_main')
  const known = projections.get(payload)
  if (known && known.conversationId === record.conversationId && known.savedAt === record.savedAt
    && known.pending.length === own.length && known.pending.every((item, index) => item === own[index])) return known.conversation
  const timeline = chatTimeline(payload.messages, payload.questions, new Set())
  const folded = foldAnswers(payload.messages, payload.questions)
  const messages: Conversation['messages'] = [...timeline.messages]
  for (const item of own) if (!messages.some((message) => message.message_id === item.message_id)) {
    messages.push({ message_id: item.message_id, event_hash: item.message_id, role: 'user', text: item.text, attachments: item.attachments, created_at_ms: item.created_at_ms, state: 'sending' })
  }
  const conversation: Conversation = {
    messages, before: timeline.before, rest: timeline.rest, answers: folded.answers, hidden: folded.hidden,
    unread: null, replying: false, status: null, stoppable: false,
    blocked: '本地记录可查看，连上并同步后继续发送。', origin: { kind: 'cache', savedAt: record.savedAt },
  }
  projections.set(payload, { conversationId: record.conversationId, savedAt: record.savedAt, pending: own, conversation })
  return conversation
}
